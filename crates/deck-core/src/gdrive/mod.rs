//! Google Drive backup and restore of the user's omp configuration.
//!
//! Sign-in is Google's installed-app OAuth flow (PKCE + loopback redirect, [`oauth`]); backups
//! are `.tar.gz` archives ([`archive`]) stored in the Drive `appDataFolder` ([`drive`]), which
//! only this OAuth client can see. Token state lives in `config_dir()/google-drive.json`
//! ([`account`]). See `docs/google-drive-setup.md` for the maintainer's Google Cloud setup.

mod account;
mod archive;
mod drive;
mod oauth;
mod project;

#[cfg(test)]
mod tests;

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::{Duration, SystemTime};

pub use account::{Account, AccountStore, LastBackup};
pub use archive::{HISTORY_DB, Manifest, PreparedRestore, RestorePlan, RestoreReport};
pub use drive::{BackupFile, to_delete};
pub use oauth::{LOGIN_TIMEOUT, LoginFlow, SCOPES};
pub use project::{
    KEEP_PROJECT_BACKUPS, MemoryBank, OmpDir, PROJECT_PREFIX, Parts, PreparedProjectRestore,
    Progress, ProjectBackup, ProjectBackupOutcome, ProjectRestorePlan, ProjectRestoreReport,
    ProjectSizes, ProjectSyncState, ProjectSynced, SessionPlan, SessionSize, Stage, measure,
    path_id,
};

use crate::usage::Date;

/// Backups kept on Drive after a successful upload (newest first).
pub const KEEP_BACKUPS: usize = 30;
/// Local safety copies kept under the restore-safety directory.
pub const KEEP_SAFETY_COPIES: usize = 5;
/// File-name prefix of every backup in the app folder.
pub const BACKUP_PREFIX: &str = "omp-backup-";

const CLIENT_ID_VAR: &str = "OMP_DECK_GOOGLE_CLIENT_ID";
const CLIENT_SECRET_VAR: &str = "OMP_DECK_GOOGLE_CLIENT_SECRET";

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The refresh token was revoked or expired; the local sign-in state was removed.
    #[error("Google sign-in is no longer valid; sign in again")]
    SignedOut,
    #[error("not signed in to Google")]
    NotSignedIn,
    #[error("cannot reach Google: {0}")]
    Network(String),
    /// A non-success answer from Google, already worded for the user.
    #[error("{0}")]
    Google(String),
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

/// Google's endpoints; injectable so tests can talk to a local mock server.
#[derive(Clone, Debug)]
pub struct Endpoints {
    pub auth: String,
    pub token: String,
    pub revoke: String,
    /// Drive v3 metadata API.
    pub drive: String,
    /// Drive v3 upload API.
    pub upload: String,
}

impl Endpoints {
    pub fn google() -> Endpoints {
        Endpoints {
            auth: "https://accounts.google.com/o/oauth2/v2/auth".to_owned(),
            token: "https://oauth2.googleapis.com/token".to_owned(),
            revoke: "https://oauth2.googleapis.com/revoke".to_owned(),
            drive: "https://www.googleapis.com/drive/v3".to_owned(),
            upload: "https://www.googleapis.com/upload/drive/v3".to_owned(),
        }
    }
}

/// The OAuth client of this build. Google desktop clients need the secret, which is not
/// confidential for installed apps.
#[derive(Clone, Debug)]
pub struct Credentials {
    pub client_id: String,
    pub client_secret: Option<String>,
}

impl Credentials {
    /// Runtime env vars win over the values baked in at build time; `None` = this build has no
    /// OAuth client, so the feature is unavailable.
    pub fn detect() -> Option<Credentials> {
        Self::pick(
            std::env::var(CLIENT_ID_VAR).ok(),
            option_env!("OMP_DECK_GOOGLE_CLIENT_ID"),
            std::env::var(CLIENT_SECRET_VAR).ok(),
            option_env!("OMP_DECK_GOOGLE_CLIENT_SECRET"),
        )
    }

    fn pick(
        id: Option<String>,
        baked_id: Option<&str>,
        secret: Option<String>,
        baked_secret: Option<&str>,
    ) -> Option<Credentials> {
        let choose = |runtime: Option<String>, baked: Option<&str>| {
            runtime
                .map(|v| v.trim().to_owned())
                .filter(|v| !v.is_empty())
                .or_else(|| {
                    baked
                        .map(str::trim)
                        .filter(|v| !v.is_empty())
                        .map(str::to_owned)
                })
        };
        Some(Credentials {
            client_id: choose(id, baked_id)?,
            client_secret: choose(secret, baked_secret),
        })
    }
}

/// A finished upload.
#[derive(Clone, Debug)]
pub struct BackupOutcome {
    pub file: BackupFile,
    /// Old backups deleted by retention.
    pub pruned: usize,
}

/// Everything the backup operations need; cheap to clone onto worker threads.
#[derive(Clone, Debug)]
pub struct Service {
    pub creds: Credentials,
    pub endpoints: Endpoints,
    pub store: AccountStore,
    /// The omp agent directory whose config is backed up.
    pub agent_dir: PathBuf,
    /// Where restores keep the local safety copies.
    pub safety_root: PathBuf,
    /// Where project backups are staged (never `/tmp`: archives can be large).
    pub cache_root: PathBuf,
}

impl Service {
    /// Production wiring: Google endpoints, `config_dir()/google-drive.json`,
    /// `$XDG_DATA_HOME/omp-deck/restore-safety`.
    pub fn new(creds: Credentials, agent_dir: PathBuf) -> Service {
        Service {
            creds,
            endpoints: Endpoints::google(),
            store: AccountStore::default_location(),
            agent_dir,
            safety_root: default_safety_root(),
            cache_root: default_cache_root(),
        }
    }

    /// Binds the loopback listener and builds the consent URL (no network).
    pub fn begin_login(&self) -> Result<LoginFlow> {
        LoginFlow::start(&self.creds, &self.endpoints)
    }

    /// Waits for the browser, exchanges the code and saves the account.
    pub fn finish_login(
        &self,
        flow: LoginFlow,
        cancel: &AtomicBool,
        timeout: Duration,
    ) -> Result<Account> {
        let code = flow.wait_code(cancel, timeout)?;
        let tokens = flow.exchange(&http(), &self.creds, &self.endpoints, &code)?;
        let refresh_token = tokens.refresh_token.ok_or_else(|| {
            Error::Login("Google returned no refresh token; try signing in again".to_owned())
        })?;
        let account = Account {
            refresh_token,
            email: tokens
                .id_token
                .as_deref()
                .and_then(oauth::email_from_id_token),
            ..Account::default()
        };
        self.store.save(&account)?;
        Ok(account)
    }

    /// Revokes the refresh token at Google and deletes the local state. The local state is
    /// removed even when revoking fails; that failure is still reported.
    pub fn sign_out(&self) -> Result<()> {
        let account = self.store.load()?;
        let revoked = match &account {
            Some(a) => oauth::revoke(&http(), &self.endpoints, &a.refresh_token),
            None => Ok(()),
        };
        self.store.delete()?;
        revoked
    }

    /// Uploads a backup. Without `force` nothing happens (`Ok(None)`) when the synced content
    /// is already on Drive or there is nothing to back up.
    pub fn backup(&self, force: bool) -> Result<Option<BackupOutcome>> {
        let mut account = self.signed_in()?;
        let snap = archive::Snapshot::take(&self.agent_dir)?;
        if snap.is_empty() {
            return if force {
                Err(Error::NothingToBackUp(self.agent_dir.display().to_string()))
            } else {
                Ok(None)
            };
        }
        if !force && account.synced_fingerprint.as_deref() == Some(snap.fingerprint()) {
            return Ok(None);
        }
        let now = unix_now();
        let host = hostname();
        let bytes = snap.archive(now, &host)?;
        let name = backup_name(now, &host);
        let mut drive = self.drive(&account);
        let file = drive.upload(&name, &host, snap.fingerprint(), &bytes)?;
        account.synced_fingerprint = Some(snap.fingerprint().to_owned());
        account.last_backup = Some(LastBackup {
            name: file.name.clone(),
            unix: now,
            host,
        });
        self.store.save(&account)?;
        // Retention is housekeeping: the backup itself already succeeded.
        let pruned = match self.prune(&mut drive) {
            Ok(n) => n,
            Err(e) => {
                log::warn!("pruning old backups: {e}");
                0
            }
        };
        Ok(Some(BackupOutcome { file, pruned }))
    }

    /// All backups, newest first.
    pub fn list(&self) -> Result<Vec<BackupFile>> {
        let account = self.signed_in()?;
        let mut files = self.drive(&account).list()?;
        drive::sort_newest_first(&mut files);
        Ok(files)
    }

    /// Downloads and validates `file` and works out what restoring it would change. Nothing
    /// local is touched.
    pub fn prepare_restore(&self, file: &BackupFile) -> Result<PreparedRestore> {
        let account = self.signed_in()?;
        let bytes = self.drive(&account).download(&file.id)?;
        PreparedRestore::new(
            &bytes,
            &self.agent_dir,
            &self.safety_root,
            file.name.clone(),
            unix_now(),
        )
    }

    /// Applies a prepared restore (safety copy first) and records its content as synced.
    pub fn apply_restore(&self, prepared: &PreparedRestore) -> Result<RestoreReport> {
        let report = prepared.apply(&self.agent_dir)?;
        if let Some(mut account) = self.store.load()? {
            account.synced_fingerprint = Some(prepared.manifest.fingerprint.clone());
            self.store.save(&account)?;
        }
        Ok(report)
    }

    fn signed_in(&self) -> Result<Account> {
        self.store.load()?.ok_or(Error::NotSignedIn)
    }

    fn drive(&self, account: &Account) -> drive::Drive<'_> {
        drive::Drive::new(self, account.refresh_token.clone())
    }

    /// Deletes everything beyond the newest [`KEEP_BACKUPS`]; returns how many were deleted.
    fn prune(&self, drive: &mut drive::Drive<'_>) -> Result<usize> {
        let files = drive.list()?;
        let doomed = to_delete(&files, KEEP_BACKUPS);
        for f in &doomed {
            drive.delete(&f.id)?;
        }
        Ok(doomed.len())
    }
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
        .timeout_connect(Some(Duration::from_secs(15)))
        .timeout_global(Some(Duration::from_secs(600)))
        .user_agent(concat!("omp-deck/", env!("CARGO_PKG_VERSION")))
        .build()
        .into()
}

/// The blocking HTTP client for big transfers: no global deadline (a multi-GB transfer is
/// legitimately long), a stall timeout instead. Uploads never follow redirects, so a resumable
/// session's `308` arrives as data.
pub(crate) fn transfer_http(follow_redirects: bool) -> ureq::Agent {
    let stall = Some(Duration::from_secs(60));
    let mut config = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_connect(Some(Duration::from_secs(15)))
        .timeout_global(None)
        .timeout_per_read(stall)
        .timeout_per_write(stall)
        .user_agent(concat!("omp-deck/", env!("CARGO_PKG_VERSION")));
    if !follow_redirects {
        config = config.max_redirects(0);
    }
    config.build().into()
}

fn network(e: ureq::Error) -> Error {
    Error::Network(e.to_string())
}

/// A readable error for a non-success answer to `what`. Understands Drive's
/// `{"error":{"message"}}` and OAuth's `{"error","error_description"}` bodies.
fn google_error(what: &str, status: u16, body: &str) -> Error {
    let json: serde_json::Value = serde_json::from_str(body).unwrap_or_default();
    let err = &json["error"];
    let detail = err["message"]
        .as_str()
        .or_else(|| json["error_description"].as_str())
        .or_else(|| err.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let msg = match (status, detail) {
        (429 | 500..=599, _) => {
            format!("{what} failed: Google is busy or unavailable (HTTP {status}); try again later")
        }
        (_, Some(d)) => format!("{what} failed: {d} (HTTP {status})"),
        (_, None) => format!("{what} failed (HTTP {status})"),
    };
    Error::Google(msg)
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
