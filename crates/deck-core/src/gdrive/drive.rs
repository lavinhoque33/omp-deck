//! Drive v3 client limited to the `appDataFolder`: list, multipart upload, download, delete.

use std::cmp::Ordering;
use std::collections::BTreeMap;

use serde::{Deserialize, Deserializer};
use ureq::Body;
use ureq::http::Response;

use super::{BACKUP_PREFIX, Error, Result, Service, google_error, network, oauth};
use crate::usage::parse_iso_ms;

/// Downloads larger than this are refused (a backup is config + prompt history).
const MAX_DOWNLOAD: u64 = 1 << 30;
const LIST_FIELDS: &str = "nextPageToken,files(id,name,size,createdTime,appProperties)";
const FILE_FIELDS: &str = "id,name,size,createdTime,appProperties";

/// One backup in the app folder.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct BackupFile {
    pub id: String,
    pub name: String,
    #[serde(default, deserialize_with = "size_string")]
    pub size: u64,
    /// RFC 3339, as Drive reports it.
    #[serde(rename = "createdTime", default)]
    pub created: String,
    #[serde(rename = "appProperties", default)]
    pub props: BTreeMap<String, String>,
}

impl BackupFile {
    /// The machine that uploaded it.
    pub fn host(&self) -> Option<&str> {
        self.props.get("host").map(String::as_str)
    }

    /// Creation time, unix seconds.
    pub fn created_unix(&self) -> Option<i64> {
        parse_iso_ms(&self.created).map(|ms| ms.div_euclid(1000))
    }
}

/// Drive sends `size` as a decimal string.
fn size_string<'de, D: Deserializer<'de>>(d: D) -> std::result::Result<u64, D::Error> {
    Ok(Option::<String>::deserialize(d)?
        .and_then(|s| s.parse().ok())
        .unwrap_or(0))
}

/// Newest first by creation time, then name (names carry a UTC timestamp).
pub(super) fn sort_newest_first(files: &mut [BackupFile]) {
    files.sort_by(newest_first);
}

fn newest_first(a: &BackupFile, b: &BackupFile) -> Ordering {
    (b.created_unix(), &b.name).cmp(&(a.created_unix(), &a.name))
}

/// Retention: the backups to delete so that only the newest `keep` remain.
pub fn to_delete(files: &[BackupFile], keep: usize) -> Vec<&BackupFile> {
    let mut sorted: Vec<&BackupFile> = files.iter().collect();
    sorted.sort_by(|a, b| newest_first(a, b));
    sorted.split_off(keep.min(sorted.len()))
}

pub(super) struct Drive<'a> {
    service: &'a Service,
    http: ureq::Agent,
    refresh_token: String,
    access: Option<String>,
}

impl<'a> Drive<'a> {
    pub(super) fn new(service: &'a Service, refresh_token: String) -> Drive<'a> {
        Drive {
            service,
            http: super::http(),
            refresh_token,
            access: None,
        }
    }

    fn token(&mut self) -> Result<String> {
        if let Some(t) = &self.access {
            return Ok(t.clone());
        }
        let result = oauth::refresh(
            &self.http,
            &self.service.creds,
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

    /// Sends with a bearer token; a 401 refreshes the token once and retries. Non-success
    /// answers become readable errors about `what`.
    fn authed(
        &mut self,
        what: &str,
        send: impl Fn(&ureq::Agent, &str) -> std::result::Result<Response<Body>, ureq::Error>,
    ) -> Result<Response<Body>> {
        let auth = format!("Bearer {}", self.token()?);
        let mut resp = send(&self.http, &auth).map_err(network)?;
        if resp.status() == 401 {
            self.access = None;
            let auth = format!("Bearer {}", self.token()?);
            resp = send(&self.http, &auth).map_err(network)?;
        }
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

    /// Every backup in the app folder (unordered).
    pub(super) fn list(&mut self) -> Result<Vec<BackupFile>> {
        #[derive(Deserialize)]
        struct Page {
            #[serde(default)]
            files: Vec<BackupFile>,
            #[serde(rename = "nextPageToken")]
            next: Option<String>,
        }
        let url = format!("{}/files", self.service.endpoints.drive);
        let query = format!("name contains '{BACKUP_PREFIX}' and trashed = false");
        let mut out = Vec::new();
        let mut page_token: Option<String> = None;
        loop {
            let resp = self.authed("Listing backups", |http, auth| {
                let mut req = http
                    .get(&url)
                    .header("Authorization", auth)
                    .query("spaces", "appDataFolder")
                    .query("q", &query)
                    .query("fields", LIST_FIELDS)
                    .query("pageSize", "100");
                if let Some(t) = &page_token {
                    req = req.query("pageToken", t);
                }
                req.call()
            })?;
            let page: Page = read_json(resp, "Listing backups")?;
            out.extend(
                page.files
                    .into_iter()
                    .filter(|f| f.name.starts_with(BACKUP_PREFIX)),
            );
            match page.next {
                Some(t) if !t.is_empty() => page_token = Some(t),
                _ => return Ok(out),
            }
        }
    }

    /// Multipart upload into the app folder.
    pub(super) fn upload(
        &mut self,
        name: &str,
        host: &str,
        fingerprint: &str,
        bytes: &[u8],
    ) -> Result<BackupFile> {
        let metadata = serde_json::json!({
            "name": name,
            "parents": ["appDataFolder"],
            "mimeType": "application/gzip",
            "appProperties": { "host": host, "fingerprint": fingerprint },
        });
        let boundary = format!("omp-deck-{}", fingerprint.get(..24).unwrap_or(fingerprint));
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

    pub(super) fn download(&mut self, id: &str) -> Result<Vec<u8>> {
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

    pub(super) fn delete(&mut self, id: &str) -> Result<()> {
        let url = self.file_url(id)?;
        self.authed("Deleting an old backup", |http, auth| {
            http.delete(&url).header("Authorization", auth).call()
        })
        .map(drop)
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
}

fn read_json<T: for<'de> Deserialize<'de>>(mut resp: Response<Body>, what: &str) -> Result<T> {
    let body = resp.body_mut().read_to_string().map_err(network)?;
    serde_json::from_str(&body)
        .map_err(|e| Error::Google(format!("{what} failed: unexpected answer from Drive ({e})")))
}
