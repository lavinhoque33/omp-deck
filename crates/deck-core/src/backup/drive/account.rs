//! The signed-in Google account: `config_dir()/google-drive.json`, mode 0600.

use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::backup::{LastBackup, Result, read_state, remove_state, write_state};
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
        Ok(read_state::<Account>(&self.path)?.filter(|a| !a.refresh_token.is_empty()))
    }

    /// Atomic (temp file + rename); the file is created 0600 because it holds the refresh token.
    pub fn save(&self, account: &Account) -> Result<()> {
        write_state(&self.path, account)
    }

    /// `name` in the directory of the account file (the project sync state lives beside it).
    pub(in crate::backup) fn sibling(&self, name: &str) -> PathBuf {
        self.path.with_file_name(name)
    }

    pub fn delete(&self) -> Result<()> {
        remove_state(&self.path)
    }
}
