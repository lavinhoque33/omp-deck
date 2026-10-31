//! Backup and restore of the user's omp configuration ([`archive`]) and of project folders
//! ([`project`]) to one active [`Destination`]: Google Drive ([`drive`]) or the user's own
//! FTP/FTPS/SFTP server ([`server`]). Both destinations implement [`remote::Remote`]; everything
//! here and in [`project`] talks to that trait only, so the safety checks, change detection and
//! retention are shared. Each destination keeps its own local state files (see
//! `docs/notes/contracts.md`, "Backup destinations").

mod archive;
pub mod drive;
mod project;
mod remote;
pub mod server;

#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

pub use archive::{HISTORY_DB, Manifest, PreparedRestore, RestorePlan, RestoreReport};
pub use project::{
    ConflictChoice, KEEP_PROJECT_BACKUPS, LocalCopy, MemoryBank, OmpDir, PROJECT_PREFIX, Parts,
    PreparedProjectRestore, Progress, ProjectBackup, ProjectBackupOutcome, ProjectRestorePlan,
    ProjectRestoreReport, ProjectSizes, ProjectSyncState, ProjectSynced, SessionPlan, SessionSize,
    Stage, measure, path_id,
};
pub use remote::{BackupFile, to_delete};

use crate::usage::Date;
use drive::{Account, AccountStore, Credentials, Endpoints};
use remote::Remote;
use server::{ServerAccount, ServerStore};

/// Config backups kept at the destination after a successful upload (newest first).
pub const KEEP_BACKUPS: usize = 30;
/// Local safety copies kept under the restore-safety directory.
pub const KEEP_SAFETY_COPIES: usize = 5;
/// File-name prefix of every config backup.
pub const BACKUP_PREFIX: &str = "omp-backup-";

/// Where backups go. One is active at a time; each keeps its own sign-in and sync state.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Destination {
    #[default]
    Drive,
    /// The FTP/FTPS/SFTP server saved by [`Service::server_connect`].
    Server,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The refresh token was revoked or expired; the local sign-in state was removed.
    #[error("Google sign-in is no longer valid; sign in again")]
    SignedOut,
    #[error("not signed in to Google")]
    NotSignedIn,
    /// This build has no Google OAuth client.
    #[error("Google Drive is not available in this build")]
    DriveUnavailable,
    #[error("no backup server is set up")]
    NoServer,
    #[error("cannot reach Google: {0}")]
    Network(String),
    /// A non-success answer from Google, already worded for the user.
    #[error("{0}")]
    Google(String),
    /// A failure talking to the backup server, already worded for the user.
    #[error("{0}")]
    Server(String),
    /// The server settings cannot work (bad host, folder, or auth for the protocol).
    #[error("{0}")]
    InvalidServer(String),
    /// Trust on first use: the server's SFTP host key or FTPS certificate is not pinned yet (and,
    /// for FTPS, not trusted by the WebPKI roots). Retry with [`server::ServerConfig::pinned`] set
    /// to `fingerprint` once the user confirmed it.
    #[error("the server is not trusted yet; its fingerprint is {fingerprint}")]
    UntrustedServer { fingerprint: String },
    /// The server presented a key or certificate other than the pinned one.
    #[error(
        "the server's identity changed (expected {pinned}, got {presented}); this could be an \
         attack. If the change is expected, remove the server and add it again"
    )]
    ServerChanged { pinned: String, presented: String },
    #[error("cancelled")]
    Cancelled,
    #[error("sign-in timed out: the browser never came back")]
    TimedOut,
    #[error("sign-in failed: {0}")]
    Login(String),
    #[error("{0}")]
    Io(String),
    #[error("invalid backup: {0}")]
    InvalidBackup(String),
    #[error("prompt history database: {0}")]
    History(String),
    #[error("nothing to back up in {0}")]
    NothingToBackUp(String),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// `context path: error` as an [`Error::Io`].
fn io_err(context: &str, path: &Path) -> impl FnOnce(io::Error) -> Error {
    let msg = format!("{context} {}", path.display());
    move |e| Error::Io(format!("{msg}: {e}"))
}

/// The last upload from this machine to a destination.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct LastBackup {
    /// Backup file name.
    pub name: String,
    /// Upload time, unix seconds.
    pub unix: i64,
    pub host: String,
}

/// A finished upload.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct BackupOutcome {
    pub file: BackupFile,
    /// Old backups deleted by retention.
    pub pruned: usize,
}

/// Everything the backup operations need; cheap to clone onto worker threads. Every operation
/// acts on [`Self::destination`]; [`Self::with_destination`] picks another one per job.
#[derive(Clone, Debug)]
pub struct Service {
    /// The Google OAuth client of this build; `None` = the Drive destination is unavailable.
    pub creds: Option<Credentials>,
    pub endpoints: Endpoints,
    /// The Google account (`google-drive.json`).
    pub store: AccountStore,
    /// The saved server (`backup-server.json`).
    pub server_store: ServerStore,
    pub destination: Destination,
    /// The omp agent directory whose config is backed up.
    pub agent_dir: PathBuf,
    /// Where restores keep the local safety copies.
    pub safety_root: PathBuf,
    /// Where project backups are staged (never `/tmp`: archives can be large).
    pub cache_root: PathBuf,
}

/// The active destination's local record: what is known to be there.
enum Linked {
    Drive(Account),
    Server(ServerAccount),
}

impl Linked {
    fn synced_fingerprint(&mut self) -> &mut Option<String> {
        match self {
            Linked::Drive(a) => &mut a.synced_fingerprint,
            Linked::Server(s) => &mut s.synced_fingerprint,
        }
    }

    fn last_backup(&mut self) -> &mut Option<LastBackup> {
        match self {
            Linked::Drive(a) => &mut a.last_backup,
            Linked::Server(s) => &mut s.last_backup,
        }
    }
}

impl Service {
    /// Production wiring: Google endpoints, `config_dir()/google-drive.json`,
    /// `config_dir()/backup-server.json`, `$XDG_DATA_HOME/omp-deck/restore-safety`; the Drive
    /// destination is active.
    pub fn new(creds: Option<Credentials>, agent_dir: PathBuf) -> Service {
        Service {
            creds,
            endpoints: Endpoints::google(),
            store: AccountStore::default_location(),
            server_store: ServerStore::default_location(),
            destination: Destination::Drive,
            agent_dir,
            safety_root: default_safety_root(),
            cache_root: default_cache_root(),
        }
    }

    /// This service acting on `destination` (a clone; the stores are shared files).
    pub fn with_destination(&self, destination: Destination) -> Service {
        Service {
            destination,
            ..self.clone()
        }
    }

    /// Uploads a backup. Without `force` nothing happens (`Ok(None)`) when the synced content
    /// is already at the destination or there is nothing to back up.
    pub fn backup(&self, force: bool) -> Result<Option<BackupOutcome>> {
        let mut linked = self.linked()?;
        let snap = archive::Snapshot::take(&self.agent_dir)?;
        if snap.is_empty() {
            return if force {
                Err(Error::NothingToBackUp(self.agent_dir.display().to_string()))
            } else {
                Ok(None)
            };
        }
        if !force && linked.synced_fingerprint().as_deref() == Some(snap.fingerprint()) {
            return Ok(None);
        }
        let now = unix_now();
        let host = hostname();
        let bytes = snap.archive(now, &host)?;
        let name = backup_name(now, &host);
        let props = BTreeMap::from([
            ("host".to_owned(), host.clone()),
            ("fingerprint".to_owned(), snap.fingerprint().to_owned()),
        ]);
        let mut remote = self.remote(&linked)?;
        let file = remote.upload_bytes(&name, &props, &bytes)?;
        *linked.synced_fingerprint() = Some(snap.fingerprint().to_owned());
        *linked.last_backup() = Some(LastBackup {
            name: file.name.clone(),
            unix: now,
            host,
        });
        self.save_linked(&linked)?;
        // Retention is housekeeping: the backup itself already succeeded.
        let pruned = match prune(remote.as_mut()) {
            Ok(n) => n,
            Err(e) => {
                log::warn!("pruning old backups: {e}");
                0
            }
        };
        Ok(Some(BackupOutcome { file, pruned }))
    }

    /// All config backups, newest first.
    pub fn list(&self) -> Result<Vec<BackupFile>> {
        let linked = self.linked()?;
        let mut files = self.remote(&linked)?.list_config()?;
        remote::sort_newest_first(&mut files);
        Ok(files)
    }

    /// Downloads and validates `file` and works out what restoring it would change. Nothing
    /// local is touched.
    pub fn prepare_restore(&self, file: &BackupFile) -> Result<PreparedRestore> {
        let linked = self.linked()?;
        let bytes = self.remote(&linked)?.download(&file.id)?;
        PreparedRestore::new(
            &bytes,
            &self.agent_dir,
            &self.safety_root,
            file.name.clone(),
            unix_now(),
        )
    }

    /// Applies a prepared restore (safety copy first) and records its content as synced with
    /// the active destination.
    pub fn apply_restore(&self, prepared: &PreparedRestore) -> Result<RestoreReport> {
        let report = prepared.apply(&self.agent_dir)?;
        if let Some(mut linked) = self.load_linked()? {
            *linked.synced_fingerprint() = Some(prepared.manifest.fingerprint.clone());
            self.save_linked(&linked)?;
        }
        Ok(report)
    }

    /// Permanently deletes the config backup `file` (bypassing any trash); a file that is already
    /// gone counts as deleted. What the destination holds under `file.id` is checked first: only
    /// a config backup of this app named `file.name` (the files [`Self::list`] returns) is
    /// deleted, never a project backup or anything else.
    ///
    /// When the deleted file is this computer's last backup, or carries the fingerprint recorded
    /// as synced (uploaded or restored here), that record is dropped so the next automatic pass
    /// uploads the current config again.
    pub fn delete_backup(&self, file: &BackupFile) -> Result<()> {
        let refuse = |name: &str| {
            Err(Error::InvalidBackup(format!(
                "{name} is not a config backup; refusing to delete it"
            )))
        };
        if !remote::is_config_backup(file) {
            return refuse(&file.name);
        }
        let linked = self.linked()?;
        let mut remote = self.remote(&linked)?;
        if let Some(actual) = remote.metadata(&file.id)? {
            if !remote::is_config_backup(&actual) || actual.name != file.name {
                return refuse(&actual.name);
            }
            remote.delete_permanently(&file.id)?;
        }
        // Reload: the record may have changed meanwhile (e.g. a refreshed sign-in).
        let Some(mut linked) = self.load_linked()? else {
            return Ok(());
        };
        let was_last = linked
            .last_backup()
            .as_ref()
            .is_some_and(|l| l.name == file.name);
        let was_synced = linked
            .synced_fingerprint()
            .as_deref()
            .is_some_and(|fp| file.props.get("fingerprint").map(String::as_str) == Some(fp));
        if was_last || was_synced {
            if was_last {
                *linked.last_backup() = None;
            }
            *linked.synced_fingerprint() = None;
            self.save_linked(&linked)?;
        }
        Ok(())
    }

    /// The active destination's record; `None` = not signed in / no server saved.
    fn load_linked(&self) -> Result<Option<Linked>> {
        match self.destination {
            Destination::Drive => {
                self.creds()?;
                Ok(self.store.load()?.map(Linked::Drive))
            }
            Destination::Server => Ok(self.server_store.load()?.map(Linked::Server)),
        }
    }

    fn linked(&self) -> Result<Linked> {
        self.load_linked()?.ok_or(match self.destination {
            Destination::Drive => Error::NotSignedIn,
            Destination::Server => Error::NoServer,
        })
    }

    fn save_linked(&self, linked: &Linked) -> Result<()> {
        match linked {
            Linked::Drive(a) => self.store.save(a),
            Linked::Server(s) => self.server_store.save(s),
        }
    }

    /// A client for the destination of `linked`. A server is connected and logged in here.
    fn remote(&self, linked: &Linked) -> Result<Box<dyn Remote + '_>> {
        Ok(match linked {
            Linked::Drive(a) => Box::new(drive::client::Drive::new(self, a.refresh_token.clone())),
            Linked::Server(s) => Box::new(server::open(&s.config)?),
        })
    }

    /// A failure detected in what the active destination answered, worded for the user.
    fn remote_failure(&self, what: &str, problem: &str) -> Error {
        match self.destination {
            Destination::Drive => Error::Google(format!("{what} failed: Drive {problem}")),
            Destination::Server => Error::Server(format!("{what} failed: the server {problem}")),
        }
    }
}

/// Deletes everything beyond the newest [`KEEP_BACKUPS`]; returns how many were deleted.
fn prune(remote: &mut dyn Remote) -> Result<usize> {
    let files = remote.list_config()?;
    let doomed = to_delete(&files, KEEP_BACKUPS);
    for f in &doomed {
        remote.delete(&f.id)?;
    }
    Ok(doomed.len())
}

/// A local state file: `None` when missing; a corrupt file counts as missing (logged).
fn read_state<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<Option<T>> {
    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(io_err("cannot read", path)(e)),
    };
    match serde_json::from_slice(&bytes) {
        Ok(v) => Ok(Some(v)),
        Err(e) => {
            log::warn!("corrupt {}: {e}; ignoring it", path.display());
            Ok(None)
        }
    }
}

/// Saves a local state file atomically, created private (0600): it may hold secrets.
fn write_state<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let json = serde_json::to_vec_pretty(value).map_err(|e| Error::Io(e.to_string()))?;
    write_private(path, &json)
}

/// Deletes a local state file; a missing one is fine.
fn remove_state(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(io_err("cannot delete", path)(e)),
        _ => Ok(()),
    }
}

/// Writes `bytes` to `path` atomically (sibling temp file + rename), created private (0600).
fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    crate::platform::write_private(path, bytes).map_err(io_err("cannot write", path))
}

/// `omp-deck/restore-safety` under the platform data directory (`dirs::data_dir()`).
fn default_safety_root() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_default()
        .join("omp-deck")
        .join("restore-safety")
}

/// `omp-deck/project-sync` under the platform cache directory (`dirs::cache_dir()`): where project
/// backups are staged while they are built, uploaded, downloaded and verified.
fn default_cache_root() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_default()
        .join("omp-deck")
        .join("project-sync")
}

/// The blocking HTTP client: statuses are inspected by the caller, never turned into errors.
pub(crate) fn http() -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_connect(Some(CONNECT_TIMEOUT))
        .timeout_global(Some(Duration::from_secs(600)))
        .user_agent(concat!("omp-deck/", env!("CARGO_PKG_VERSION")))
        .build()
        .into()
}

/// Connecting to a destination gives up after this long.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// A transfer that moves no byte for this long has stalled.
const STALL_TIMEOUT: Duration = Duration::from_secs(60);

/// The blocking HTTP client for big transfers: no global deadline (a multi-GB transfer is
/// legitimately long), a stall timeout instead. Uploads never follow redirects, so a resumable
/// session's `308` arrives as data.
pub(crate) fn transfer_http(follow_redirects: bool) -> ureq::Agent {
    let stall = Some(STALL_TIMEOUT);
    let mut config = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_connect(Some(CONNECT_TIMEOUT))
        .timeout_global(None)
        .timeout_per_read(stall)
        .timeout_per_write(stall)
        .user_agent(concat!("omp-deck/", env!("CARGO_PKG_VERSION")));
    if !follow_redirects {
        config = config.max_redirects(0);
    }
    config.build().into()
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0))
}

/// This machine's hostname (`unknown` when unreadable).
pub fn hostname() -> String {
    crate::platform::hostname().unwrap_or_else(|| "unknown".to_owned())
}

/// UTC calendar parts of a unix time.
fn utc(unix: i64) -> (Date, i64, i64, i64) {
    let secs = unix.rem_euclid(86_400);
    (
        Date::from_days(unix.div_euclid(86_400)),
        secs / 3_600,
        secs % 3_600 / 60,
        secs % 60,
    )
}

/// `2026-10-22T13:52:01Z`.
fn rfc3339(unix: i64) -> String {
    let (d, h, m, s) = utc(unix);
    format!(
        "{:04}-{:02}-{:02}T{h:02}:{m:02}:{s:02}Z",
        d.year, d.month, d.day
    )
}

/// `20261022T135201Z`: sorts chronologically, safe in file names.
fn compact_stamp(unix: i64) -> String {
    let (d, h, m, s) = utc(unix);
    format!(
        "{:04}{:02}{:02}T{h:02}{m:02}{s:02}Z",
        d.year, d.month, d.day
    )
}

/// `omp-backup-<UTC timestamp>-<host>.tar.gz`, the host reduced to file-name-safe characters.
fn backup_name(unix: i64, host: &str) -> String {
    let host: String = host
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '-'
            }
        })
        .collect();
    format!("{BACKUP_PREFIX}{}-{host}.tar.gz", compact_stamp(unix))
}
