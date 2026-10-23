//! The signed-in Google account: `config_dir()/google-drive.json`, mode 0600.

use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::{Error, Result, io_err};
use crate::config::config_dir;

#[derive(Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Account {
    pub refresh_token: String,
    /// From the id_token of the sign-in (`None` when Google sent none).
    pub email: Option<String>,
    /// The last upload from this machine.
    pub last_backup: Option<LastBackup>,
    /// Content fingerprint of the synced set known to be on Drive (uploaded or restored here);
    /// auto backup uploads only when the local content differs from it.
    pub synced_fingerprint: Option<String>,
}

// Hand-written so the refresh token never ends up in a log.
impl fmt::Debug for Account {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Account")
            .field("email", &self.email)
            .field("last_backup", &self.last_backup)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct LastBackup {
    /// Drive file name.
    pub name: String,
    /// Upload time, unix seconds.
    pub unix: i64,
    pub host: String,
}

#[derive(Clone, Debug)]
pub struct AccountStore {
    path: PathBuf,
}

impl AccountStore {
    pub fn default_location() -> AccountStore {
        AccountStore::at(config_dir().join("google-drive.json"))
    }

    pub fn at(path: PathBuf) -> AccountStore {
        AccountStore { path }
    }

    /// `None` = signed out. A corrupt file counts as signed out (logged).
    pub fn load(&self) -> Result<Option<Account>> {
        let bytes = match fs::read(&self.path) {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(io_err("cannot read", &self.path)(e)),
        };
        match serde_json::from_slice::<Account>(&bytes) {
            Ok(a) if !a.refresh_token.is_empty() => Ok(Some(a)),
            Ok(_) => Ok(None),
            Err(e) => {
                log::warn!(
                    "corrupt {}: {e}; treating as signed out",
                    self.path.display()
                );
                Ok(None)
            }
        }
    }

    /// Atomic (temp file + rename); the file is created 0600 because it holds the refresh token.
    pub fn save(&self, account: &Account) -> Result<()> {
        let json = serde_json::to_vec_pretty(account).map_err(|e| Error::Io(e.to_string()))?;
        write_private(&self.path, &json)
    }

    /// `name` in the directory of the account file (the project sync state lives beside it).
    pub(super) fn sibling(&self, name: &str) -> PathBuf {
        self.path.with_file_name(name)
    }

    pub fn delete(&self) -> Result<()> {
        match fs::remove_file(&self.path) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => {
                Err(io_err("cannot delete", &self.path)(e))
            }
            _ => Ok(()),
        }
    }
}

/// Writes `bytes` to `path` atomically (sibling temp file + rename), created private (0600).
pub(super) fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    crate::platform::write_private(path, bytes).map_err(io_err("cannot write", path))
}
