//! Drive v3 client limited to the `appDataFolder`: list, multipart and resumable upload,
//! streaming download, delete.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde::Deserialize;
use ureq::Body;
use ureq::http::Response;

use super::{google_error, network, oauth};
use crate::backup::remote::{
    BackupFile, MAX_DOWNLOAD, PROJECT_KIND, Remote, is_config_backup, is_project_backup,
};
use crate::backup::{BACKUP_PREFIX, Error, Result, Service, http, transfer_http};

const LIST_FIELDS: &str = "nextPageToken,files(id,name,size,createdTime,appProperties)";
const FILE_FIELDS: &str = "id,name,size,createdTime,appProperties";
/// Resumable upload chunk: a multiple of 256 KiB, as Drive requires for all but the last.
const CHUNK: usize = 8 << 20;
/// Consecutive failed attempts before a transfer gives up.
const MAX_ATTEMPTS: u32 = 5;

/// Which HTTP agent a request uses: the API agent has a global timeout; the transfer agents
/// only a stall timeout (a multi-GB transfer is legitimately long).
#[derive(Clone, Copy)]
enum Lane {
    Api,
    /// Never follows redirects: resumable uploads answer `308` as data.
    Upload,
    Download,
}

pub(in crate::backup) struct Drive<'a> {
    service: &'a Service,
    http: ureq::Agent,
    upload: ureq::Agent,
    download: ureq::Agent,
    refresh_token: String,
    access: Option<String>,
}

impl<'a> Drive<'a> {
    pub(in crate::backup) fn new(service: &'a Service, refresh_token: String) -> Drive<'a> {
        Drive {
            service,
            http: http(),
            upload: transfer_http(false),
            download: transfer_http(true),
            refresh_token,
            access: None,
        }
    }

    pub(in crate::backup) fn token(&mut self) -> Result<String> {
        if let Some(t) = &self.access {
            return Ok(t.clone());
        }
        let creds = self.service.creds.as_ref().ok_or(Error::DriveUnavailable)?;
        let result = oauth::refresh(
            &self.http,
            creds,
            &self.service.endpoints,
            &self.refresh_token,
        );
        if matches!(result, Err(Error::SignedOut)) {
            // The grant is gone for good: drop the local state so the UI shows "signed out".
            self.service.store.delete()?;
        }
        let t = result?;
        self.access = Some(t.clone());
        Ok(t)
    }

    /// A request with a bearer token; a 401 refreshes the token once and retries. The answer is
    /// returned whatever its status (resumable uploads read 308 as data).
    fn raw(
        &mut self,
        lane: Lane,
        send: impl Fn(&ureq::Agent, &str) -> std::result::Result<Response<Body>, ureq::Error>,
    ) -> Result<Response<Body>> {
        let agent = self.agent(lane).clone();
        let auth = format!("Bearer {}", self.token()?);
        let mut resp = send(&agent, &auth).map_err(network)?;
        if resp.status() == 401 {
            self.access = None;
            let auth = format!("Bearer {}", self.token()?);
            resp = send(&agent, &auth).map_err(network)?;
        }
        Ok(resp)
    }

    /// Like [`Self::raw`] on the API lane; non-success answers become readable errors about
    /// `what`.
    fn authed(
        &mut self,
        what: &str,
        send: impl Fn(&ureq::Agent, &str) -> std::result::Result<Response<Body>, ureq::Error>,
    ) -> Result<Response<Body>> {
        self.authed_on(Lane::Api, what, send)
    }

    fn authed_on(
        &mut self,
        lane: Lane,
        what: &str,
        send: impl Fn(&ureq::Agent, &str) -> std::result::Result<Response<Body>, ureq::Error>,
    ) -> Result<Response<Body>> {
        let mut resp = self.raw(lane, send)?;
        let status = resp.status().as_u16();
        if (200..300).contains(&status) {
            return Ok(resp);
        }
        let body = resp.body_mut().read_to_string().unwrap_or_default();
        Err(match status {
            404 => Error::Google(format!(
                "{what} failed: the backup no longer exists on Drive"
            )),
            _ => google_error(what, status, &body),
        })
    }

    fn agent(&self, lane: Lane) -> &ureq::Agent {
        match lane {
            Lane::Api => &self.http,
            Lane::Upload => &self.upload,
            Lane::Download => &self.download,
        }
    }

    /// Every backup in the app folder (unordered).
    pub(in crate::backup) fn list(&mut self) -> Result<Vec<BackupFile>> {
        let query = format!("name contains '{BACKUP_PREFIX}' and trashed = false");
        let mut files = self.list_where(&query, "Listing backups")?;
        files.retain(is_config_backup);
        Ok(files)
    }

    /// All pages of the app-folder files matching the Drive query `query`.
    fn list_where(&mut self, query: &str, what: &str) -> Result<Vec<BackupFile>> {
        #[derive(Deserialize)]
        struct Page {
            #[serde(default)]
            files: Vec<BackupFile>,
            #[serde(rename = "nextPageToken")]
            next: Option<String>,
        }
        let url = format!("{}/files", self.service.endpoints.drive);
        let mut out = Vec::new();
        let mut page_token: Option<String> = None;
        loop {
            let resp = self.authed(what, |http, auth| {
                let mut req = http
                    .get(&url)
                    .header("Authorization", auth)
                    .query("spaces", "appDataFolder")
                    .query("q", query)
                    .query("fields", LIST_FIELDS)
                    .query("pageSize", "100");
                if let Some(t) = &page_token {
                    req = req.query("pageToken", t);
                }
                req.call()
            })?;
            let page: Page = read_json(resp, what)?;
            out.extend(page.files);
            match page.next {
                Some(t) if !t.is_empty() => page_token = Some(t),
                _ => return Ok(out),
            }
        }
    }

    /// Multipart upload into the app folder.
    pub(in crate::backup) fn upload(
        &mut self,
        name: &str,
        props: &BTreeMap<String, String>,
        bytes: &[u8],
    ) -> Result<BackupFile> {
        let metadata = serde_json::json!({
            "name": name,
            "parents": ["appDataFolder"],
            "mimeType": "application/gzip",
            "appProperties": props,
        });
        let tag = props.get("fingerprint").map_or(name, String::as_str);
        let boundary = format!("omp-deck-{}", tag.get(..24).unwrap_or(tag));
        let mut body = Vec::with_capacity(bytes.len() + 512);
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Type: application/json; charset=UTF-8\r\n\r\n{metadata}\r\n\
                 --{boundary}\r\nContent-Type: application/gzip\r\n\r\n"
            )
            .as_bytes(),
        );
        body.extend_from_slice(bytes);
        body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
        let url = format!("{}/files", self.service.endpoints.upload);
        let content_type = format!("multipart/related; boundary={boundary}");
        let resp = self.authed("Uploading the backup", |http, auth| {
            http.post(&url)
                .header("Authorization", auth)
                .content_type(&content_type)
                .query("uploadType", "multipart")
                .query("fields", FILE_FIELDS)
                .send(&body[..])
        })?;
        read_json(resp, "Uploading the backup")
    }

    pub(in crate::backup) fn download(&mut self, id: &str) -> Result<Vec<u8>> {
        let url = self.file_url(id)?;
        let mut resp = self.authed("Downloading the backup", |http, auth| {
            http.get(&url)
                .header("Authorization", auth)
                .query("alt", "media")
                .call()
        })?;
        resp.body_mut()
            .with_config()
            .limit(MAX_DOWNLOAD)
            .read_to_vec()
            .map_err(network)
    }

    /// The metadata Drive holds for `id`; `None` when the file no longer exists (404).
    pub(in crate::backup) fn metadata(&mut self, id: &str) -> Result<Option<BackupFile>> {
        const WHAT: &str = "Reading the backup's details";
        let url = self.file_url(id)?;
        let mut resp = self.raw(Lane::Api, |http, auth| {
            http.get(&url)
                .header("Authorization", auth)
                .query("fields", FILE_FIELDS)
                .call()
        })?;
        let status = resp.status().as_u16();
        if status == 404 {
            return Ok(None);
        }
        if !(200..300).contains(&status) {
            let body = resp.body_mut().read_to_string().unwrap_or_default();
            return Err(google_error(WHAT, status, &body));
        }
        read_json(resp, WHAT).map(Some)
    }

    pub(in crate::backup) fn delete(&mut self, id: &str) -> Result<()> {
        let url = self.file_url(id)?;
        self.authed("Deleting an old backup", |http, auth| {
            http.delete(&url).header("Authorization", auth).call()
        })
        .map(drop)
    }

    /// Permanent `files.delete` (bypasses the trash). A file that is already gone (404) counts as
    /// deleted. Network errors and 429/5xx answers are retried ([`MAX_ATTEMPTS`] in a row, backing
    /// off).
    pub(in crate::backup) fn delete_permanently(&mut self, id: &str) -> Result<()> {
        const WHAT: &str = "Deleting the backup";
        let url = self.file_url(id)?;
        let never = AtomicBool::new(false);
        let mut failures = 0u32;
        loop {
            let resp = self.raw(Lane::Api, |http, auth| {
                http.delete(&url).header("Authorization", auth).call()
            });
            let reason = match resp {
                Err(Error::Network(m)) => m,
                Err(e) => return Err(e),
                Ok(mut resp) => {
                    let status = resp.status().as_u16();
                    if (200..300).contains(&status) || status == 404 {
                        return Ok(());
                    }
                    if !matches!(status, 429 | 500..=599) {
                        let body = resp.body_mut().read_to_string().unwrap_or_default();
                        return Err(google_error(WHAT, status, &body));
                    }
                    format!("Google answered HTTP {status}")
                }
            };
            failures += 1;
            if failures >= MAX_ATTEMPTS {
                return Err(Error::Network(reason));
            }
            log::warn!("{WHAT}: {reason}; retrying (attempt {failures})");
            backoff(failures, &never)?;
        }
    }

    /// Drive ids are URL-safe tokens; anything else is refused rather than escaped.
    fn file_url(&self, id: &str) -> Result<String> {
        if id.is_empty()
            || !id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
        {
            return Err(Error::Google(format!("invalid Drive file id “{id}”")));
        }
        Ok(format!("{}/files/{id}", self.service.endpoints.drive))
    }

    /// Resumable upload of `total` bytes of `file` into the app folder (8 MiB chunks).
    ///
    /// Network errors and 5xx answers are retried (`bytes */total` asks Drive how much it has,
    /// then the upload continues from there) up to [`MAX_ATTEMPTS`] times in a row; an expired
    /// session (404/410) is restarted once. `progress` gets the acknowledged byte count.
    pub(in crate::backup) fn upload_file(
        &mut self,
        name: &str,
        props: &BTreeMap<String, String>,
        file: &mut File,
        total: u64,
        cancel: &AtomicBool,
        progress: &mut dyn FnMut(u64),
    ) -> Result<BackupFile> {
        const WHAT: &str = "Uploading the backup";
        let mut restarted = false;
        let mut buf: Vec<u8> = Vec::new();
        'session: loop {
            let session = self.start_session(name, props, total, cancel)?;
            let mut offset = 0u64;
            let mut failures = 0u32;
            let mut query = false;
            loop {
                if cancel.load(Ordering::Relaxed) {
                    return Err(Error::Cancelled);
                }
                let sent = if query {
                    0
                } else {
                    (total - offset).min(CHUNK as u64) as usize
                };
                if !query {
                    buf.resize(sent, 0);
                    file.seek(SeekFrom::Start(offset))
                        .and_then(|_| file.read_exact(&mut buf))
                        .map_err(|e| Error::Io(format!("cannot read the archive: {e}")))?;
                }
                let range = if query {
                    format!("bytes */{total}")
                } else {
                    format!("bytes {offset}-{}/{total}", offset + sent as u64 - 1)
                };
                let resp = self.raw(Lane::Upload, |http, auth| {
                    http.put(&session)
                        .header("Authorization", auth)
                        .header("Content-Range", &range)
                        .send(&buf[..sent])
                });
                let reason = match classify_chunk(resp, WHAT)? {
                    Chunk::Done(file) => {
                        progress(total);
                        return Ok(file);
                    }
                    Chunk::Next(next) if query && next >= total => {
                        "Drive reports the upload complete but sent no file".to_owned()
                    }
                    Chunk::Next(next)
                        if next > if query { total } else { offset + sent as u64 } =>
                    {
                        return Err(Error::Google(format!(
                            "{WHAT} failed: Drive acknowledged bytes that were never sent"
                        )));
                    }
                    Chunk::Next(next) if next > offset => {
                        failures = 0;
                        offset = next;
                        query = offset >= total;
                        progress(offset);
                        continue;
                    }
                    // The answer to "where are you?": resume from wherever Drive stands.
                    Chunk::Next(next) if query => {
                        offset = next;
                        query = false;
                        continue;
                    }
                    Chunk::Next(_) => "Drive acknowledged no data".to_owned(),
                    Chunk::Expired if restarted => {
                        return Err(Error::Google(format!(
                            "{WHAT} failed: the upload session expired twice"
                        )));
                    }
                    Chunk::Expired => {
                        restarted = true;
                        continue 'session;
                    }
                    Chunk::Retry(reason) => reason,
                };
                failures += 1;
                if failures >= MAX_ATTEMPTS {
                    return Err(Error::Network(reason));
                }
                log::warn!("{WHAT}: {reason}; resuming (attempt {failures})");
                backoff(failures, cancel)?;
                query = true;
            }
        }
    }

    /// Opens a resumable session; returns its URI. Network errors and 429/5xx answers are
    /// retried like chunk failures ([`MAX_ATTEMPTS`] in a row, backing off, cancellable).
    fn start_session(
        &mut self,
        name: &str,
        props: &BTreeMap<String, String>,
        total: u64,
        cancel: &AtomicBool,
    ) -> Result<String> {
        const WHAT: &str = "Starting the upload";
        let metadata = serde_json::json!({
            "name": name,
            "parents": ["appDataFolder"],
            "mimeType": "application/gzip",
            "appProperties": props,
        })
        .to_string();
        let url = format!("{}/files", self.service.endpoints.upload);
        let mut failures = 0u32;
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            let resp = self.raw(Lane::Upload, |http, auth| {
                http.post(&url)
                    .header("Authorization", auth)
                    .content_type("application/json; charset=UTF-8")
                    .header("X-Upload-Content-Type", "application/gzip")
                    .header("X-Upload-Content-Length", total.to_string())
                    .query("uploadType", "resumable")
                    .query("fields", FILE_FIELDS)
                    .send(metadata.as_bytes())
            });
            let reason = match resp {
                Err(Error::Network(m)) => m,
                Err(e) => return Err(e),
                Ok(mut resp) => {
                    let status = resp.status().as_u16();
                    if (200..300).contains(&status) {
                        return resp
                            .headers()
                            .get("location")
                            .and_then(|v| v.to_str().ok())
                            .map(str::to_owned)
                            .ok_or_else(|| {
                                Error::Google(format!(
                                    "{WHAT} failed: Drive sent no upload address"
                                ))
                            });
                    }
                    if !matches!(status, 429 | 500..=599) {
                        let body = resp.body_mut().read_to_string().unwrap_or_default();
                        return Err(google_error(WHAT, status, &body));
                    }
                    format!("Google answered HTTP {status}")
                }
            };
            failures += 1;
            if failures >= MAX_ATTEMPTS {
                return Err(Error::Network(reason));
            }
            log::warn!("{WHAT}: {reason}; retrying (attempt {failures})");
            backoff(failures, cancel)?;
        }
    }

    /// Streams file `id` (`size` bytes) into `out`, resuming with a `Range` request after a
    /// dropped connection, a network error or a 429/5xx answer. [`MAX_ATTEMPTS`] failures in a
    /// row end the download; a request that delivered bytes resets the count. `progress` gets
    /// the byte count.
    pub(in crate::backup) fn download_to(
        &mut self,
        id: &str,
        size: u64,
        out: &mut File,
        cancel: &AtomicBool,
        progress: &mut dyn FnMut(u64),
    ) -> Result<()> {
        const WHAT: &str = "Downloading the backup";
        let url = self.file_url(id)?;
        let mut offset = 0u64;
        // The furthest absolute offset ever reached, across rewinds.
        let mut high = 0u64;
        let mut failures = 0u32;
        let mut buf = vec![0u8; 256 * 1024];
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            let resp = self.raw(Lane::Download, |http, auth| {
                let mut req = http
                    .get(&url)
                    .header("Authorization", auth)
                    .query("alt", "media");
                if offset > 0 {
                    req = req.header("Range", format!("bytes={offset}-"));
                }
                req.call()
            });
            let mut resp = match resp {
                Ok(r) => r,
                Err(Error::Network(m)) => {
                    failures += 1;
                    if failures >= MAX_ATTEMPTS {
                        return Err(Error::Network(m));
                    }
                    log::warn!("{WHAT}: {m}; retrying (attempt {failures})");
                    backoff(failures, cancel)?;
                    continue;
                }
                Err(e) => return Err(e),
            };
            let status = resp.status().as_u16();
            if !(200..300).contains(&status) {
                if matches!(status, 429 | 500..=599) {
                    failures += 1;
                    if failures >= MAX_ATTEMPTS {
                        return Err(Error::Network(format!("Google answered HTTP {status}")));
                    }
                    log::warn!("{WHAT}: HTTP {status}; retrying (attempt {failures})");
                    backoff(failures, cancel)?;
                    continue;
                }
                let body = resp.body_mut().read_to_string().unwrap_or_default();
                return Err(if status == 404 {
                    Error::Google(format!(
                        "{WHAT} failed: the backup no longer exists on Drive"
                    ))
                } else {
                    google_error(WHAT, status, &body)
                });
            }
            if offset > 0 && status == 200 {
                // The server ignored the range: start over.
                offset = 0;
                out.set_len(0)
                    .and_then(|()| out.seek(SeekFrom::Start(0)).map(drop))
                    .map_err(|e| Error::Io(format!("cannot rewind the download: {e}")))?;
            }
            let mut body = resp.body_mut().as_reader();
            let dropped = loop {
                if cancel.load(Ordering::Relaxed) {
                    return Err(Error::Cancelled);
                }
                match body.read(&mut buf) {
                    Ok(0) => break None,
                    Ok(n) => {
                        out.write_all(&buf[..n])
                            .map_err(|e| Error::Io(format!("cannot write the download: {e}")))?;
                        offset += n as u64;
                        progress(offset);
                    }
                    Err(e) => break Some(e.to_string()),
                }
            };
            if offset == size && dropped.is_none() {
                return Ok(());
            }
            if offset > size {
                return Err(Error::Google(format!(
                    "{WHAT} failed: Drive sent more than the {size} bytes expected"
                )));
            }
            if offset > high {
                // Went beyond anything completed before: only then is it forward progress
                // (a server that ignores Range and cuts at the same byte never gets past it).
                failures = 0;
                high = offset;
            }
            failures += 1;
            if failures >= MAX_ATTEMPTS {
                return Err(Error::Network(dropped.unwrap_or_else(|| {
                    format!("the download ended after {offset} of {size} bytes")
                })));
            }
            backoff(failures, cancel)?;
        }
    }

    /// Every project backup in the app folder (unordered).
    pub(in crate::backup) fn list_projects(&mut self) -> Result<Vec<BackupFile>> {
        let query = format!(
            "appProperties has {{ key='kind' and value='{PROJECT_KIND}' }} and trashed = false"
        );
        let mut files = self.list_where(&query, "Listing project backups")?;
        files.retain(is_project_backup);
        Ok(files)
    }
}

impl Remote for Drive<'_> {
    fn check(&mut self) -> Result<()> {
        self.token().map(drop)
    }

    fn list_config(&mut self) -> Result<Vec<BackupFile>> {
        self.list()
    }

    fn list_projects(&mut self) -> Result<Vec<BackupFile>> {
        Drive::list_projects(self)
    }

    fn upload_bytes(
        &mut self,
        name: &str,
        props: &BTreeMap<String, String>,
        bytes: &[u8],
    ) -> Result<BackupFile> {
        self.upload(name, props, bytes)
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
        Drive::upload_file(self, name, props, file, total, cancel, progress)
    }

    fn download(&mut self, id: &str) -> Result<Vec<u8>> {
        Drive::download(self, id)
    }

    fn download_to(
        &mut self,
        id: &str,
        size: u64,
        out: &mut File,
        cancel: &AtomicBool,
        progress: &mut dyn FnMut(u64),
    ) -> Result<()> {
        Drive::download_to(self, id, size, out, cancel, progress)
    }

    fn metadata(&mut self, id: &str) -> Result<Option<BackupFile>> {
        Drive::metadata(self, id)
    }

    fn delete(&mut self, id: &str) -> Result<()> {
        Drive::delete(self, id)
    }

    fn delete_permanently(&mut self, id: &str) -> Result<()> {
        Drive::delete_permanently(self, id)
    }
}

fn read_json<T: for<'de> Deserialize<'de>>(mut resp: Response<Body>, what: &str) -> Result<T> {
    let body = resp.body_mut().read_to_string().map_err(network)?;
    serde_json::from_str(&body)
        .map_err(|e| Error::Google(format!("{what} failed: unexpected answer from Drive ({e})")))
}

/// What Drive answered to a chunk (or to a `bytes */total` status query).
enum Chunk {
    /// The upload finished.
    Done(BackupFile),
    /// `308`: Drive has this many bytes; continue there.
    Next(u64),
    /// `404`/`410`: the session is gone.
    Expired,
    /// Network error or 5xx/429: ask Drive where it stands and continue.
    Retry(String),
}

fn classify_chunk(resp: Result<Response<Body>>, what: &str) -> Result<Chunk> {
    let mut resp = match resp {
        Ok(r) => r,
        Err(Error::Network(m)) => return Ok(Chunk::Retry(m)),
        Err(e) => return Err(e),
    };
    let status = resp.status().as_u16();
    match status {
        200 | 201 => Ok(Chunk::Done(read_json(resp, what)?)),
        308 => {
            // `Range: bytes=0-N` = N + 1 bytes stored; no header = none.
            let next = resp
                .headers()
                .get("range")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.rsplit('-').next())
                .and_then(|n| n.trim().parse::<u64>().ok())
                .map_or(0, |last| last + 1);
            Ok(Chunk::Next(next))
        }
        404 | 410 => Ok(Chunk::Expired),
        429 | 500..=599 => Ok(Chunk::Retry(format!("Google answered HTTP {status}"))),
        _ => {
            let body = resp.body_mut().read_to_string().unwrap_or_default();
            Err(google_error(what, status, &body))
        }
    }
}

/// Sleeps `base · 2^(attempt-1)` (≤ 16 · base), waking early on cancel.
fn backoff(attempt: u32, cancel: &AtomicBool) -> Result<()> {
    /// Tests do not wait half a second per retry.
    const BASE_MS: u64 = if cfg!(test) { 10 } else { 500 };
    let wait = Duration::from_millis(BASE_MS << (attempt - 1).min(4));
    let end = std::time::Instant::now() + wait;
    while std::time::Instant::now() < end {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    Ok(())
}
