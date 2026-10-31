//! What [`super::Service`] needs from a backup destination ([`Remote`]), and the file record every
//! destination lists ([`BackupFile`]).

use std::cmp::Ordering as CmpOrdering;
use std::collections::BTreeMap;
use std::fs::File;
use std::sync::atomic::AtomicBool;

use serde::{Deserialize, Deserializer};

use super::{BACKUP_PREFIX, PROJECT_PREFIX, Result};
use crate::usage::parse_iso_ms;

/// Whole-file downloads larger than this are refused (a config backup is config + prompt history).
pub(super) const MAX_DOWNLOAD: u64 = 1 << 30;
/// The `kind` property of a project backup.
pub(super) const PROJECT_KIND: &str = "project";

/// One backup at a destination. On Drive `id` is the Drive file id; on a server it is the
/// archive's file name.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, serde::Serialize)]
pub struct BackupFile {
    pub id: String,
    pub name: String,
    #[serde(default, deserialize_with = "size_string")]
    pub size: u64,
    /// RFC 3339.
    #[serde(rename = "createdTime", default)]
    pub created: String,
    /// Drive appProperties; a server backup's sidecar `props`.
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

/// Drive sends `size` as a decimal string; our own serialization (gateway replies) writes a
/// number.
fn size_string<'de, D: Deserializer<'de>>(d: D) -> std::result::Result<u64, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Size {
        Number(u64),
        Text(String),
    }
    Ok(match Option::<Size>::deserialize(d)? {
        Some(Size::Number(n)) => n,
        Some(Size::Text(s)) => s.parse().unwrap_or(0),
        None => 0,
    })
}

/// Newest first by creation time, then name (names carry a UTC timestamp).
pub(super) fn sort_newest_first(files: &mut [BackupFile]) {
    files.sort_by(newest_first);
}

fn newest_first(a: &BackupFile, b: &BackupFile) -> CmpOrdering {
    (b.created_unix(), &b.name).cmp(&(a.created_unix(), &a.name))
}

/// Retention: the backups to delete so that only the newest `keep` remain.
pub fn to_delete(files: &[BackupFile], keep: usize) -> Vec<&BackupFile> {
    let mut sorted: Vec<&BackupFile> = files.iter().collect();
    sorted.sort_by(|a, b| newest_first(a, b));
    sorted.split_off(keep.min(sorted.len()))
}

/// A config backup of this app, as [`Remote::list_config`] keeps them: an [`BACKUP_PREFIX`] name
/// and no `kind` property (project backups carry `kind = project`; config uploads set only `host`
/// and `fingerprint`).
pub(super) fn is_config_backup(f: &BackupFile) -> bool {
    f.name.starts_with(BACKUP_PREFIX) && !f.props.contains_key("kind")
}

/// A project backup of this app, as [`Remote::list_projects`] keeps them.
pub(super) fn is_project_backup(f: &BackupFile) -> bool {
    f.name.starts_with(PROJECT_PREFIX)
        && f.props.get("kind").map(String::as_str) == Some(PROJECT_KIND)
}

/// A connected backup destination. Blocking; used from worker threads.
pub(super) trait Remote {
    /// Pre-flight: proves the destination is reachable and the sign-in valid before a long job
    /// packs anything.
    fn check(&mut self) -> Result<()>;
    /// Every config backup (unordered).
    fn list_config(&mut self) -> Result<Vec<BackupFile>>;
    /// Every project backup (unordered).
    fn list_projects(&mut self) -> Result<Vec<BackupFile>>;
    /// Stores a small backup held in memory.
    fn upload_bytes(
        &mut self,
        name: &str,
        props: &BTreeMap<String, String>,
        bytes: &[u8],
    ) -> Result<BackupFile>;
    /// Streams `total` bytes of `file`; `progress` gets the byte count stored so far.
    fn upload_file(
        &mut self,
        name: &str,
        props: &BTreeMap<String, String>,
        file: &mut File,
        total: u64,
        cancel: &AtomicBool,
        progress: &mut dyn FnMut(u64),
    ) -> Result<BackupFile>;
    /// The whole backup `id` (at most [`MAX_DOWNLOAD`] bytes).
    fn download(&mut self, id: &str) -> Result<Vec<u8>>;
    /// Streams the `size` bytes of `id` into `out`; `progress` gets the byte count.
    fn download_to(
        &mut self,
        id: &str,
        size: u64,
        out: &mut File,
        cancel: &AtomicBool,
        progress: &mut dyn FnMut(u64),
    ) -> Result<()>;
    /// What the destination holds under `id`; `None` when it no longer exists.
    fn metadata(&mut self, id: &str) -> Result<Option<BackupFile>>;
    /// Retention delete.
    fn delete(&mut self, id: &str) -> Result<()>;
    /// A user-requested delete: bypasses any trash; a file that is already gone counts as
    /// deleted.
    fn delete_permanently(&mut self, id: &str) -> Result<()>;
}
