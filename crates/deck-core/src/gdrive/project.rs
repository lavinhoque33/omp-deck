//! Per-project backups: a project folder's `<folder>/.omp` directory and its omp sessions, kept on
//! Drive as one `.tar.gz` per backup (see `docs/notes/contracts.md`, "Wave 10", and
//! `docs/notes/project-sync.md`). Independent of the config backup in the sibling modules: own
//! file prefix, own Drive query, own local state file, own retention.
//!
//! * [`snapshot`]: what a project consists of, found with metadata only (sizes, fingerprint).
//! * [`pack`]: builds the archive in the cache directory, streaming, manifest last.
//! * [`restore`]: downloads, validates, plans and applies. Sessions are written only by
//!   `omp session unpack`.

mod pack;
mod restore;
mod snapshot;

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, DirBuilder, File};
use std::io::{self, Read};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::account::write_private;
use super::archive::{ManifestFile, hex};
use super::drive::{BackupFile, PROJECT_KIND, to_delete};
use super::{Error, LastBackup, Result, Service, compact_stamp, hostname, io_err, unix_now};
use crate::omp::Omp;
use crate::sessions::SessionsRoot;
use snapshot::Snapshot;

/// File-name prefix of every project backup in the app folder.
pub const PROJECT_PREFIX: &str = "omp-project-";
/// Project backups kept per (host, [`path_id`]) after an upload (newest first).
pub const KEEP_PROJECT_BACKUPS: usize = 3;

/// Archive format version (`manifest.json` `format`).
const FORMAT: u32 = 1;
/// `manifest.json` `kind`.
const KIND: &str = "project";
/// Staging directories older than this are leftovers of a crashed job.
const STALE_STAGING: Duration = Duration::from_secs(24 * 3600);
/// Progress is reported at most once per this many bytes.
const REPORT_STEP: u64 = 1 << 20;
/// omp refuses pack members above 2 GiB (`session pack`/`unpack`).
const MAX_MEMBER: u64 = 2 << 30;
const STATE_FILE: &str = "project-sync.json";

/// Which parts of a project a backup or restore covers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Parts {
    /// `<folder>/.omp`.
    pub omp: bool,
    /// The project's omp sessions.
    pub sessions: bool,
}

impl Parts {
    pub fn any(self) -> bool {
        self.omp || self.sessions
    }
}

/// The project's `.omp` directory as found on disk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OmpDir {
    Missing,
    /// The folder is `$HOME`: its `.omp` is omp's global directory, not a project one.
    Global,
    Present {
        files: u64,
        bytes: u64,
    },
    /// `.omp` is a symbolic link: never followed, not backed up.
    Symlink,
}

/// One session of a project as the Sync page lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionSize {
    pub id: String,
    /// Title, else first-prompt snippet, else "(untitled)".
    pub title: String,
    pub modified: SystemTime,
    /// The jsonl file plus its artifacts.
    pub bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectSizes {
    pub omp: OmpDir,
    /// Sessions whose header `cwd` is the project.
    pub sessions: u64,
    /// Their jsonl files plus artifacts.
    pub session_bytes: u64,
    /// Every one of those sessions, newest first.
    pub session_list: Vec<SessionSize>,
}

/// Blocking (worker). Sessions = the `sessions::scan(root)` group whose `cwd` equals `project`.
pub fn measure(root: &SessionsRoot, project: &Path) -> io::Result<ProjectSizes> {
    let both = Parts {
        omp: true,
        sessions: true,
    };
    let snap = Snapshot::take(
        root,
        project,
        both,
        &BTreeSet::new(),
        dirs::home_dir().as_deref(),
    )?;
    Ok(snap.sizes())
}

/// Lexical identity of a project path: sha256 of its bytes, first 16 hex digits. The path is
/// taken by components (as `Path` equality does), so `/a/b/` and `/a/b` are one project.
pub fn path_id(project: &Path) -> String {
    let normalized: PathBuf = project.components().collect();
    let digest = Sha256::digest(normalized.as_os_str().as_bytes());
    hex(&digest[..8])
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    /// Reading the project (items = sessions).
    Scanning,
    /// Building the archive (bytes).
    Packing,
    Uploading,
    Downloading,
    /// Unpacking and checking a downloaded archive (bytes of the archive read).
    Verifying,
    /// Writing `.omp` files (items = files).
    RestoringOmp,
    /// `omp session unpack` per session (items = sessions).
    RestoringSessions,
}

/// Progress of the running stage, in bytes or items depending on [`Stage`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Progress {
    pub stage: Stage,
    pub done: u64,
    pub total: u64,
}

/// Per-path local sync state, `config_dir()/project-sync.json` (0600, atomic write); separate
/// from `google-drive.json` so the two Drive lanes never race on one file.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct ProjectSyncState {
    pub projects: BTreeMap<PathBuf, ProjectSynced>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ProjectSynced {
    pub fingerprint: String,
    pub parts: Parts,
    pub last: LastBackup,
    /// Blobs the backed-up sessions reference that were missing or damaged then (hex → their
    /// state then): a later change makes the project count as changed.
    #[serde(default)]
    pub unresolved_blobs: BTreeMap<String, String>,
}

/// One Drive file, described by its appProperties (nothing is downloaded).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectBackup {
    pub file: BackupFile,
    pub host: String,
    pub path_id: String,
    /// The project's absolute path on the uploading machine (`…` + tail when it was too long).
    pub path: String,
    /// The folder's name.
    pub name: String,
    pub omp_bytes: Option<u64>,
    pub sessions: Option<u64>,
    pub session_bytes: Option<u64>,
}

impl ProjectBackup {
    /// `None` for files that are not valid project backups (logged).
    pub(super) fn from_file(file: BackupFile) -> Option<ProjectBackup> {
        let prop = |k: &str| file.props.get(k).filter(|v| !v.is_empty()).cloned();
        let num = |k: &str| file.props.get(k).and_then(|v| v.parse::<u64>().ok());
        let (Some(host), Some(path_id)) = (prop("host"), prop("pid")) else {
            log::warn!("project backup {} has no host/pid; ignored", file.name);
            return None;
        };
        let name = prop("name").unwrap_or_else(|| file.name.clone());
        Some(ProjectBackup {
            host,
            path_id,
            path: prop("path").unwrap_or_default(),
            name,
            omp_bytes: num("omp"),
            sessions: num("sess"),
            session_bytes: num("sessb"),
            file,
        })
    }
}

#[derive(Clone, Debug)]
pub struct ProjectBackupOutcome {
    pub backup: ProjectBackup,
    pub synced: ProjectSynced,
    /// Older backups of the same project deleted by retention.
    pub pruned: usize,
    /// Left out of the backup, `<what>: <reason>`: `.omp` entries (symlinks, odd names, special
    /// files, too deep) and sessions omp could not restore (> 2 GiB members, no complete line).
    pub skipped: Vec<String>,
}

/// A session the restore would add.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionPlan {
    pub id: String,
    pub title: String,
    pub modified_unix: i64,
    /// Session file plus artifacts.
    pub bytes: u64,
    /// The id already exists in the sessions store: the copy is added under a fresh id.
    pub new_id: bool,
    /// Why this session cannot be restored (a member above omp's 2 GiB pack limit); applying
    /// reports it as failed without running omp.
    pub problem: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProjectRestorePlan {
    pub target: PathBuf,
    /// The target folder does not exist; applying creates it.
    pub create_target: bool,
    pub omp_exists: bool,
    /// Paths relative to `.omp`.
    pub omp_overwritten: Vec<String>,
    pub omp_added: Vec<String>,
    /// Files that will NOT be written, `<path>: <reason>`: a symlink on the way or as the file
    /// itself, or a file where the backup has a folder (and the other way round).
    pub omp_skipped: Vec<String>,
    pub sessions: Vec<SessionPlan>,
}

/// A downloaded, verified backup waiting to be applied. Owns its staging directory, removed on
/// drop (also when the restore is abandoned).
#[derive(Debug)]
pub struct PreparedProjectRestore {
    pub backup: ProjectBackup,
    pub plan: ProjectRestorePlan,
    parts: Parts,
    manifest: Manifest,
    root: SessionsRoot,
    stage: Staging,
}

#[derive(Clone, Debug, Default)]
pub struct ProjectRestoreReport {
    pub omp_written: usize,
    pub omp_overwritten: usize,
    /// Where overwritten `.omp` files were copied first.
    pub safety_dir: Option<PathBuf>,
    /// `.omp` files not written because of a symlink or a file/folder clash, `<path>: <reason>`.
    pub omp_skipped: Vec<String>,
    /// The session files omp created.
    pub sessions_added: Vec<PathBuf>,
    /// (title or id, reason)
    pub sessions_failed: Vec<(String, String)>,
    /// The job was cancelled after some of the work was done; the lists above hold what was.
    pub cancelled: bool,
}

// ---------------------------------------------------------------- manifest

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Manifest {
    format: u32,
    kind: String,
    /// RFC 3339 UTC.
    created: String,
    host: String,
    #[serde(default)]
    app_version: String,
    #[serde(default)]
    omp_version: String,
    /// The project's absolute path on the uploading machine.
    project: String,
    name: String,
    fingerprint: String,
    omp: Option<OmpManifest>,
    sessions: Option<SessionsManifest>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct OmpManifest {
    files: Vec<ManifestFile>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct SessionsManifest {
    items: Vec<ItemManifest>,
    blobs: Vec<BlobManifest>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ItemManifest {
    id: String,
    file_name: String,
    title: String,
    modified_unix: i64,
    size: u64,
    sha256: String,
    artifacts: Vec<ManifestFile>,
    /// Lowercase hex of the blobs this session references (all listed in `blobs`).
    blobs: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct BlobManifest {
    sha256: String,
    size: u64,
}

fn invalid(msg: impl Into<String>) -> Error {
    Error::InvalidBackup(msg.into())
}

// ---------------------------------------------------------------- shared plumbing

/// Message of the I/O error that carries a cancel through readers and writers. Not
/// `ErrorKind::Interrupted`: `io::copy` and friends retry that forever.
const CANCELLED: &str = "cancelled";

fn cancelled_io() -> io::Error {
    io::Error::other(CANCELLED)
}
/// Counts bytes/items for one stage, honours cancel and reports progress at most once per
/// [`REPORT_STEP`] bytes (items always report).
struct Pump<'a> {
    cancel: &'a AtomicBool,
    progress: &'a (dyn Fn(Progress) + Sync),
    stage: Stage,
    total: u64,
    done: u64,
    reported: u64,
}

impl<'a> Pump<'a> {
    fn new(
        cancel: &'a AtomicBool,
        progress: &'a (dyn Fn(Progress) + Sync),
        stage: Stage,
        total: u64,
    ) -> Pump<'a> {
        let pump = Pump {
            cancel,
            progress,
            stage,
            total,
            done: 0,
            reported: 0,
        };
        pump.report();
        pump
    }

    fn report(&self) {
        (self.progress)(Progress {
            stage: self.stage,
            done: self.done,
            total: self.total,
        });
    }

    fn check_cancel(&self) -> io::Result<()> {
        if self.cancel.load(Ordering::Relaxed) {
            return Err(cancelled_io());
        }
        Ok(())
    }

    /// `n` more bytes were processed.
    fn advance(&mut self, n: u64) -> io::Result<()> {
        self.check_cancel()?;
        self.done += n;
        if self.done - self.reported >= REPORT_STEP {
            self.reported = self.done;
            self.report();
        }
        Ok(())
    }

    /// One more item was processed (always reported).
    fn item(&mut self) -> Result<()> {
        if self.cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        self.done += 1;
        self.reported = self.done;
        self.report();
        Ok(())
    }

    /// Reports the final state of a byte stage: everything done.
    fn finish(&mut self) {
        self.done = self.done.max(self.total);
        if self.reported != self.done {
            self.reported = self.done;
            self.report();
        }
    }
}

/// A reader that feeds a [`Pump`] and optionally a sha256 with everything read through it.
struct Meter<'p, 'a, R> {
    inner: R,
    pump: &'p mut Pump<'a>,
    hasher: Option<Sha256>,
    count: u64,
}

impl<'p, 'a, R: Read> Meter<'p, 'a, R> {
    fn new(inner: R, pump: &'p mut Pump<'a>, hash: bool) -> Self {
        Meter {
            inner,
            pump,
            hasher: hash.then(Sha256::new),
            count: 0,
        }
    }

    /// The sha256 of what was read (empty when not hashing).
    fn digest(&mut self) -> String {
        self.hasher
            .take()
            .map(|h| hex(&h.finalize()))
            .unwrap_or_default()
    }
}

impl<R: Read> Read for Meter<'_, '_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        if let Some(h) = &mut self.hasher {
            h.update(&buf[..n]);
        }
        self.count += n as u64;
        self.pump.advance(n as u64)?;
        Ok(n)
    }
}

/// An I/O failure of a job: [`Error::Cancelled`] when the job was cancelled, else `context`.
fn job_io(cancel: &AtomicBool, context: &str) -> impl FnOnce(io::Error) -> Error {
    let cancelled = cancel.load(Ordering::Relaxed);
    let context = context.to_owned();
    move |e| {
        if cancelled || e.to_string() == CANCELLED {
            Error::Cancelled
        } else {
            Error::Io(format!("{context}: {e}"))
        }
    }
}

/// A private scratch directory under the cache root, removed on drop.
#[derive(Debug)]
struct Staging {
    dir: PathBuf,
}

impl Staging {
    /// Creates `<root>/<random>` (0700) and first removes stale leftovers of crashed jobs.
    fn create(root: &Path) -> Result<Staging> {
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(root)
            .map_err(io_err("cannot create", root))?;
        sweep_stale(root);
        let mut random = [0u8; 8];
        getrandom::fill(&mut random)
            .map_err(|e| Error::Io(format!("no system randomness: {e}")))?;
        let dir = root.join(hex(&random));
        DirBuilder::new()
            .mode(0o700)
            .create(&dir)
            .map_err(io_err("cannot create", &dir))?;
        Ok(Staging { dir })
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        if let Err(e) = fs::remove_dir_all(&self.dir) {
            log::warn!("cannot remove staging {}: {e}", self.dir.display());
        }
    }
}

fn sweep_stale(root: &Path) {
    let Ok(rd) = fs::read_dir(root) else {
        return;
    };
    for entry in rd.flatten() {
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| SystemTime::now().duration_since(t).ok())
            .is_some_and(|age| age > STALE_STAGING);
        if old && let Err(e) = fs::remove_dir_all(entry.path()) {
            log::warn!(
                "cannot remove stale staging {}: {e}",
                entry.path().display()
            );
        }
    }
}

/// Longest prefix of `s` of at most `max` bytes, cut on a character boundary.
fn fit_head(s: &str, max: usize) -> &str {
    let mut end = max.min(s.len());
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Longest suffix of `s` of at most `max` bytes, cut on a character boundary.
fn fit_tail(s: &str, max: usize) -> &str {
    let mut start = s.len().saturating_sub(max);
    while !s.is_char_boundary(start) {
        start += 1;
    }
    &s[start..]
}

/// Drive allows 124 bytes per appProperty, key and value together.
const PROP_LIMIT: usize = 124;

fn put_prop(props: &mut BTreeMap<String, String>, key: &str, value: &str) {
    props.insert(
        key.to_owned(),
        fit_head(value, PROP_LIMIT - key.len()).to_owned(),
    );
}

/// `<folder>/…/tail` when the path does not fit.
fn path_prop(path: &str) -> String {
    let max = PROP_LIMIT - "path".len();
    if path.len() <= max {
        path.to_owned()
    } else {
        format!("…{}", fit_tail(path, max - '…'.len_utf8()))
    }
}

fn file_safe(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// `omp-project-<UTC timestamp>-<host>-<slug>.tar.gz`; the slug is the folder name (≤ 40).
fn project_backup_name(unix: i64, host: &str, project: &Path) -> String {
    let folder = project
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let slug: String = file_safe(&folder).chars().take(40).collect();
    let slug = if slug.is_empty() { "project" } else { &slug };
    format!(
        "{PROJECT_PREFIX}{}-{}-{slug}.tar.gz",
        compact_stamp(unix),
        file_safe(host)
    )
}

fn same_path(a: &Path, b: &Path) -> bool {
    a == b || matches!((fs::canonicalize(a), fs::canonicalize(b)), (Ok(x), Ok(y)) if x == y)
}

/// What the store holds under blob name `hex` right now: `missing` or `<len>:<mtime ns>`.
/// Metadata only; compared with the state recorded when the blob could not be archived.
fn blob_state(blob_dir: &Path, hex: &str) -> String {
    match fs::symlink_metadata(blob_dir.join(hex)) {
        Ok(m) => {
            let mtime = m
                .modified()
                .ok()
                .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_nanos());
            format!("{}:{mtime}", m.len())
        }
        Err(_) => "missing".to_owned(),
    }
}

// ---------------------------------------------------------------- Service

impl Service {
    /// Local sync state; missing or corrupt → default (corrupt is logged).
    pub fn project_state(&self) -> ProjectSyncState {
        let path = self.store.sibling(STATE_FILE);
        match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
                log::warn!("corrupt {}: {e}; starting empty", path.display());
                ProjectSyncState::default()
            }),
            Err(e) => {
                if e.kind() != io::ErrorKind::NotFound {
                    log::warn!("cannot read {}: {e}", path.display());
                }
                ProjectSyncState::default()
            }
        }
    }

    fn save_project_state(&self, state: &ProjectSyncState) -> Result<()> {
        let json = serde_json::to_vec_pretty(state).map_err(|e| Error::Io(e.to_string()))?;
        write_private(&self.store.sibling(STATE_FILE), &json)
    }

    /// Backs up the chosen `parts` of `project`; sessions whose id is in `excluded` are left out
    /// (silently, and the set is part of the change detection). `Ok(None)` = unchanged since the
    /// last backup of this path with these parts (skipped unless `force`). Nothing selected or
    /// nothing left to back up is [`Error::NothingToBackUp`].
    #[allow(clippy::too_many_arguments)] // the contract's signature
    pub fn backup_project(
        &self,
        root: &SessionsRoot,
        project: &Path,
        parts: Parts,
        excluded: &BTreeSet<String>,
        force: bool,
        cancel: &AtomicBool,
        progress: &(dyn Fn(Progress) + Sync),
    ) -> Result<Option<ProjectBackupOutcome>> {
        let account = self.signed_in()?;
        let nothing = || Error::NothingToBackUp(project.display().to_string());
        if !parts.any() {
            return Err(nothing());
        }
        progress(Progress {
            stage: Stage::Scanning,
            done: 0,
            total: 0,
        });
        let snap = Snapshot::take(root, project, parts, excluded, dirs::home_dir().as_deref())
            .map_err(io_err("cannot read", project))?;
        if snap.is_empty() {
            return Err(nothing());
        }
        let known = self.project_state().projects.remove(project);
        if !force
            && known.as_ref().is_some_and(|k| {
                k.fingerprint == snap.fingerprint()
                    && k.parts == parts
                    && k.unresolved_blobs
                        .iter()
                        .all(|(h, state)| blob_state(&snap.blob_dir, h) == *state)
            })
        {
            return Ok(None);
        }
        // Fail fast: offline or signed-out ends the job before anything is packed.
        let mut drive = self.drive(&account);
        drive.token()?;
        let stage = Staging::create(&self.cache_root)?;
        let now = unix_now();
        let host = hostname();
        let name = project_backup_name(now, &host, project);
        let omp_version = if snap.sessions_included() {
            Omp::detect()
                .and_then(|o| o.version().ok())
                .unwrap_or_else(|| "unknown".to_owned())
        } else {
            "unknown".to_owned()
        };
        let archive = stage.dir.join("backup.tar.gz");
        let packed = pack::pack(
            &snap,
            &archive,
            &pack::Meta {
                unix: now,
                host: &host,
                project,
                omp_version: &omp_version,
            },
            cancel,
            progress,
        )?;

        let folder = project
            .file_name()
            .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        let path_text = project.to_string_lossy();
        let mut props = BTreeMap::new();
        put_prop(&mut props, "kind", PROJECT_KIND);
        put_prop(&mut props, "host", &host);
        put_prop(&mut props, "pid", &path_id(project));
        put_prop(&mut props, "fp", snap.fingerprint());
        put_prop(&mut props, "name", &folder);
        props.insert("path".to_owned(), path_prop(&path_text));
        for (key, value) in [
            ("omp", packed.omp_bytes),
            ("sess", packed.sessions),
            ("sessb", packed.session_bytes),
        ] {
            if let Some(v) = value {
                put_prop(&mut props, key, &v.to_string());
            }
        }

        let mut file = File::open(&archive).map_err(io_err("cannot read", &archive))?;
        let total = packed.len;
        progress(Progress {
            stage: Stage::Uploading,
            done: 0,
            total,
        });
        let uploaded = drive.upload_file(&name, &props, &mut file, total, cancel, &mut |done| {
            progress(Progress {
                stage: Stage::Uploading,
                done,
                total,
            });
        })?;
        if uploaded.size != 0 && uploaded.size != total {
            return Err(Error::Google(format!(
                "Uploading the backup failed: Drive stored {} bytes instead of {total}",
                uploaded.size
            )));
        }
        // The state seen while packing, not a fresh sample: a blob that changed since is then
        // noticed as a change.
        let unresolved_blobs = packed.unresolved.clone();
        let mut skipped = if parts.omp {
            snap.skipped.clone()
        } else {
            Vec::new()
        };
        if parts.sessions {
            skipped.extend(snap.session_notes.iter().cloned());
        }
        skipped.extend(packed.excluded.iter().cloned());
        for what in &skipped {
            log::warn!("project backup of {}: left out {what}", project.display());
        }
        let synced = ProjectSynced {
            fingerprint: snap.fingerprint().to_owned(),
            parts,
            last: LastBackup {
                name: uploaded.name.clone(),
                unix: now,
                host: host.clone(),
            },
            unresolved_blobs,
        };
        let mut state = self.project_state();
        state.projects.insert(project.to_owned(), synced.clone());
        self.save_project_state(&state)?;
        // Retention is housekeeping: the backup itself already succeeded.
        let pid = path_id(project);
        let pruned = match prune_project(&mut drive, &host, &pid) {
            Ok(n) => n,
            Err(e) => {
                log::warn!("pruning old project backups: {e}");
                0
            }
        };
        let backup = ProjectBackup {
            file: uploaded,
            host,
            path_id: pid,
            path: props.remove("path").unwrap_or_default(),
            name: props.remove("name").unwrap_or_default(),
            omp_bytes: packed.omp_bytes,
            sessions: packed.sessions,
            session_bytes: packed.session_bytes,
        };
        Ok(Some(ProjectBackupOutcome {
            backup,
            synced,
            pruned,
            skipped,
        }))
    }

    /// Permanently deletes `backup` from Drive (`files.delete` bypasses the trash); a file that is
    /// already gone counts as deleted. Only project backups are accepted. When it is the last
    /// backup recorded for a project, that project's local state is dropped so the next pass
    /// uploads it again.
    pub fn delete_project_backup(&self, backup: &ProjectBackup) -> Result<()> {
        if backup.file.props.get("kind").map(String::as_str) != Some(PROJECT_KIND) {
            return Err(invalid(format!(
                "{} is not a project backup; refusing to delete it",
                backup.file.name
            )));
        }
        let account = self.signed_in()?;
        self.drive(&account).delete_permanently(&backup.file.id)?;
        let mut state = self.project_state();
        let before = state.projects.len();
        state
            .projects
            .retain(|_, synced| synced.last.name != backup.file.name);
        if state.projects.len() != before {
            self.save_project_state(&state)?;
        }
        Ok(())
    }

    /// All project backups, newest first.
    pub fn list_projects(&self) -> Result<Vec<ProjectBackup>> {
        let account = self.signed_in()?;
        let mut files = self.drive(&account).list_projects()?;
        super::drive::sort_newest_first(&mut files);
        Ok(files
            .into_iter()
            .filter_map(ProjectBackup::from_file)
            .collect())
    }

    /// Downloads and verifies `backup` into a staging directory and works out what restoring
    /// `parts` into `target` would change. Nothing outside the cache directory is touched.
    pub fn prepare_project_restore(
        &self,
        backup: &ProjectBackup,
        target: &Path,
        parts: Parts,
        root: &SessionsRoot,
        cancel: &AtomicBool,
        progress: &(dyn Fn(Progress) + Sync),
    ) -> Result<PreparedProjectRestore> {
        if !parts.any() {
            return Err(invalid("nothing selected to restore"));
        }
        if parts.omp && dirs::home_dir().is_some_and(|h| same_path(&h, target)) {
            return Err(Error::Io(
                "the project .omp cannot be restored into your home folder (it is omp's global directory)"
                    .to_owned(),
            ));
        }
        if backup.file.size == 0 {
            return Err(Error::Google(
                "Downloading the backup failed: Drive reports an empty file".to_owned(),
            ));
        }
        let account = self.signed_in()?;
        let stage = Staging::create(&self.cache_root)?;
        let archive = stage.dir.join("backup.tar.gz");
        let size = backup.file.size;
        {
            let mut out = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&archive)
                .map_err(io_err("cannot create", &archive))?;
            progress(Progress {
                stage: Stage::Downloading,
                done: 0,
                total: size,
            });
            self.drive(&account).download_to(
                &backup.file.id,
                size,
                &mut out,
                cancel,
                &mut |done| {
                    progress(Progress {
                        stage: Stage::Downloading,
                        done,
                        total: size,
                    });
                },
            )?;
        }
        let unpacked = stage.dir.join("x");
        let manifest = restore::extract(&archive, &unpacked, cancel, progress)?;
        if let Err(e) = fs::remove_file(&archive) {
            log::warn!("cannot remove {}: {e}", archive.display());
        }
        if parts.omp && manifest.omp.is_none() {
            return Err(invalid("this backup has no .omp directory"));
        }
        if parts.sessions && manifest.sessions.is_none() {
            return Err(invalid("this backup has no sessions"));
        }
        let plan = restore::plan(&manifest, target, parts, root)?;
        Ok(PreparedProjectRestore {
            backup: backup.clone(),
            plan,
            parts,
            manifest,
            root: root.clone(),
            stage,
        })
    }

    /// Writes a prepared restore: `.omp` files (overwritten ones copied to a safety directory
    /// first, nothing deleted, nothing written through symlinks), then every session through
    /// `omp session unpack` against the sessions store the restore was prepared for. `omp` may
    /// be `None` only when no sessions are restored. A session that fails is reported in
    /// [`ProjectRestoreReport::sessions_failed`]; the rest continue. A cancel after the first
    /// write returns `Ok` with [`ProjectRestoreReport::cancelled`] set and what was done.
    pub fn apply_project_restore(
        &self,
        prepared: PreparedProjectRestore,
        omp: Option<&Omp>,
        cancel: &AtomicBool,
        progress: &(dyn Fn(Progress) + Sync),
    ) -> Result<ProjectRestoreReport> {
        restore::apply(&prepared, &self.safety_root, omp, cancel, progress)
    }
}

/// Deletes everything beyond the newest [`KEEP_PROJECT_BACKUPS`] of one (host, path id).
fn prune_project(drive: &mut super::drive::Drive<'_>, host: &str, pid: &str) -> Result<usize> {
    let files: Vec<BackupFile> = drive
        .list_projects()?
        .into_iter()
        .filter(|f| f.props.get("pid").map(String::as_str) == Some(pid))
        .filter(|f| f.props.get("host").map(String::as_str) == Some(fit_head(host, PROP_LIMIT - 4)))
        .collect();
    let doomed = to_delete(&files, KEEP_PROJECT_BACKUPS);
    for f in &doomed {
        drive.delete(&f.id)?;
    }
    Ok(doomed.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_properties_fit_drive_limits_on_char_boundaries() {
        let long = format!("/home/{}/projekt", "ä".repeat(80));
        let p = path_prop(&long);
        assert!(p.starts_with('…') && long.ends_with(&p[3..]));
        assert!("path".len() + p.len() <= PROP_LIMIT, "{}", p.len());
        let short = "/home/u/x";
        assert_eq!(path_prop(short), short);

        let mut props = BTreeMap::new();
        put_prop(&mut props, "name", &"é".repeat(100));
        let v = &props["name"];
        assert!("name".len() + v.len() <= PROP_LIMIT);
        assert!(v.chars().all(|c| c == 'é'));
        put_prop(&mut props, "host", "box");
        assert_eq!(props["host"], "box");
    }

    #[test]
    fn names_are_file_safe_and_ids_lexical() {
        let name = project_backup_name(0, "my host/1", Path::new("/w/Proj ect: ü"));
        assert_eq!(
            name,
            "omp-project-19700101T000000Z-my-host-1-Proj-ect---.tar.gz"
        );
        let long = project_backup_name(0, "h", Path::new(&format!("/w/{}", "x".repeat(90))));
        assert!(long.ends_with(&format!("-h-{}.tar.gz", "x".repeat(40))));
        assert!(project_backup_name(0, "h", Path::new("/")).ends_with("-h-project.tar.gz"));

        let id = path_id(Path::new("/w/proj"));
        assert_eq!(id.len(), 16);
        assert!(id.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_eq!(id, path_id(Path::new("/w/proj/")), "components, not text");
        assert_ne!(id, path_id(Path::new("/w/other")));
    }
}
