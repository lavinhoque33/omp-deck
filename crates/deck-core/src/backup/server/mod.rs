//! The server destination: the user's own FTP, explicit FTPS ([`ftp`]) or SFTP ([`sftp`])
//! server. Everything lives in one remote folder as an archive plus a JSON sidecar
//! ([`layout`]). The saved server (including its password) lives in
//! `config_dir()/backup-server.json`, mode 0600; its project sync state beside it in
//! `project-sync-server.json`.

mod ftp;
mod layout;
mod sftp;

use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::{
    Destination, Error, LastBackup, Result, Service, read_state, remove_state, write_state,
};
use crate::config::config_dir;
pub(super) use layout::ServerRemote;
#[cfg(test)]
pub(super) use layout::{Sidecar, valid_id, valid_name};

/// The remote folder used when none is given (relative to the login directory).
pub const DEFAULT_FOLDER: &str = "omp-deck-backups";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    /// Plain FTP: login and backups travel unencrypted.
    Ftp,
    /// FTP upgraded with `AUTH TLS` (explicit FTPS) on the FTP port.
    Ftps,
    Sftp,
}

impl Protocol {
    pub fn default_port(self) -> u16 {
        match self {
            Protocol::Ftp | Protocol::Ftps => 21,
            Protocol::Sftp => 22,
        }
    }

    /// `ftp`, `ftps`, `sftp`.
    pub fn scheme(self) -> &'static str {
        match self {
            Protocol::Ftp => "ftp",
            Protocol::Ftps => "ftps",
            Protocol::Sftp => "sftp",
        }
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ServerAuth {
    /// FTP/FTPS only: `anonymous` login.
    Anonymous,
    Password {
        user: String,
        password: String,
    },
    /// SFTP only: an OpenSSH (or PEM) private key file.
    Key {
        user: String,
        key_path: PathBuf,
        passphrase: Option<String>,
    },
}

impl ServerAuth {
    /// The login name (`None` = anonymous).
    pub fn user(&self) -> Option<&str> {
        match self {
            ServerAuth::Anonymous => None,
            ServerAuth::Password { user, .. } | ServerAuth::Key { user, .. } => Some(user),
        }
    }
}

// Hand-written so a password or passphrase never ends up in a log.
impl fmt::Debug for ServerAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ServerAuth::Anonymous => f.write_str("Anonymous"),
            ServerAuth::Password { user, .. } => f
                .debug_struct("Password")
                .field("user", user)
                .finish_non_exhaustive(),
            ServerAuth::Key { user, key_path, .. } => f
                .debug_struct("Key")
                .field("user", user)
                .field("key_path", key_path)
                .finish_non_exhaustive(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerConfig {
    pub protocol: Protocol,
    pub host: String,
    /// `0` = the protocol's default port.
    pub port: u16,
    pub auth: ServerAuth,
    /// Remote folder; relative paths are relative to the login directory. Empty =
    /// [`DEFAULT_FOLDER`].
    pub folder: String,
    /// The trusted SFTP host key (`SHA256:<base64>`, OpenSSH format) or FTPS leaf certificate
    /// (`SHA256:<hex:hex:…>`); `None` = not pinned yet (FTPS then trusts the WebPKI roots).
    pub pinned: Option<String>,
}

impl ServerConfig {
    /// `sftp://host`, with `:port` when it is not the default; no user, no folder.
    pub fn label(&self) -> String {
        let scheme = self.protocol.scheme();
        let host = if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        if self.port == 0 || self.port == self.protocol.default_port() {
            format!("{scheme}://{host}")
        } else {
            format!("{scheme}://{host}:{}", self.port)
        }
    }

    /// The port actually dialled.
    pub fn effective_port(&self) -> u16 {
        if self.port == 0 {
            self.protocol.default_port()
        } else {
            self.port
        }
    }

    /// Trimmed and checked: a usable host, auth that fits the protocol, a folder without `..`
    /// (empty → [`DEFAULT_FOLDER`], trailing and repeated `/` dropped), the default port filled in.
    pub fn normalized(mut self) -> Result<ServerConfig> {
        let bad = |m: &str| Err(Error::InvalidServer(m.to_owned()));
        self.host = self.host.trim().to_owned();
        if let Some(inner) = self
            .host
            .strip_prefix('[')
            .and_then(|h| h.strip_suffix(']'))
        {
            self.host = inner.to_owned();
        }
        if self.host.is_empty() {
            return bad("enter the server's host name or address");
        }
        if self
            .host
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || matches!(c, '/' | '@' | '\\'))
        {
            return bad("the host name contains characters that are not allowed");
        }
        self.port = self.effective_port();
        match (&mut self.auth, self.protocol) {
            (ServerAuth::Anonymous, Protocol::Sftp) => {
                return bad("SFTP needs a user name and a password or key");
            }
            (ServerAuth::Key { .. }, Protocol::Ftp | Protocol::Ftps) => {
                return bad("key files work with SFTP only");
            }
            (ServerAuth::Password { user, .. } | ServerAuth::Key { user, .. }, _) => {
                *user = user.trim().to_owned();
                if user.is_empty() {
                    return bad("enter a user name");
                }
            }
            (ServerAuth::Anonymous, _) => {}
        }
        self.folder = normalize_folder(&self.folder)?;
        self.pinned = self
            .pinned
            .map(|p| p.trim().to_owned())
            .filter(|p| !p.is_empty());
        Ok(self)
    }

    /// Same server, login and folder: what is there is what the saved sync state describes.
    fn same_place(&self, other: &ServerConfig) -> bool {
        self.protocol == other.protocol
            && self.host.eq_ignore_ascii_case(&other.host)
            && self.effective_port() == other.effective_port()
            && self.auth.user() == other.auth.user()
            && self.folder == other.folder
    }
}

/// `a/b`, `/srv/a`; empty → [`DEFAULT_FOLDER`].
fn normalize_folder(folder: &str) -> Result<String> {
    let folder = folder.trim();
    let absolute = folder.starts_with('/');
    let mut parts = Vec::new();
    for part in folder.split('/').filter(|p| !p.is_empty()) {
        if !layout::valid_name(part) || part == "." {
            return Err(Error::InvalidServer(format!(
                "the folder name “{part}” is not allowed"
            )));
        }
        parts.push(part);
    }
    let joined = parts.join("/");
    Ok(match (absolute, joined.is_empty()) {
        (true, _) => format!("/{joined}"),
        (false, true) => DEFAULT_FOLDER.to_owned(),
        (false, false) => joined,
    })
}

/// The saved server and what this machine knows to be there.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ServerAccount {
    pub config: ServerConfig,
    /// The last upload from this machine.
    #[serde(default)]
    pub last_backup: Option<LastBackup>,
    /// Content fingerprint of the synced set known to be on the server (uploaded or restored
    /// here); auto backup uploads only when the local content differs from it.
    #[serde(default)]
    pub synced_fingerprint: Option<String>,
}

/// `config_dir()/backup-server.json`, mode 0600 (it holds the password).
#[derive(Clone, Debug)]
pub struct ServerStore {
    path: PathBuf,
}

impl ServerStore {
    pub fn default_location() -> ServerStore {
        ServerStore::at(config_dir().join("backup-server.json"))
    }

    pub fn at(path: PathBuf) -> ServerStore {
        ServerStore { path }
    }

    /// `None` = no server saved. A corrupt file counts as none (logged).
    pub fn load(&self) -> Result<Option<ServerAccount>> {
        read_state(&self.path)
    }

    pub fn save(&self, account: &ServerAccount) -> Result<()> {
        write_state(&self.path, account)
    }

    pub fn delete(&self) -> Result<()> {
        remove_state(&self.path)
    }

    /// `name` in the directory of the server file (the project sync state lives beside it).
    pub(in crate::backup) fn sibling(&self, name: &str) -> PathBuf {
        self.path.with_file_name(name)
    }
}

/// Connects and logs in to `config`'s server and enters its folder (created when missing).
pub(super) fn open(config: &ServerConfig) -> Result<ServerRemote> {
    let transport = match config.protocol {
        Protocol::Ftp => ftp::connect(config, false)?,
        Protocol::Ftps => ftp::connect(config, true)?,
        Protocol::Sftp => sftp::connect(config)?,
    };
    Ok(ServerRemote::new(transport))
}

impl Service {
    /// The saved server, if any.
    pub fn server_account(&self) -> Result<Option<ServerAccount>> {
        self.server_store.load()
    }

    /// Checks and saves `config` as the backup server: connects, logs in, creates the folder if
    /// needed and writes, renames and deletes a probe file there. Nothing is saved on failure.
    /// An unpinned server whose identity cannot be verified is
    /// [`Error::UntrustedServer`]; retry with `pinned` set to its fingerprint once the user
    /// trusts it. Replacing a server with the same host, port, login and folder keeps the sync
    /// state; anything else starts fresh (including the project sync state).
    pub fn server_connect(&self, config: ServerConfig) -> Result<ServerAccount> {
        let config = config.normalized()?;
        open(&config)?.probe()?;
        let previous = self.server_store.load()?;
        let account = match previous {
            Some(p) if p.config.same_place(&config) => ServerAccount { config, ..p },
            _ => {
                remove_state(&self.project_state_path(Destination::Server))?;
                ServerAccount {
                    config,
                    last_backup: None,
                    synced_fingerprint: None,
                }
            }
        };
        self.server_store.save(&account)?;
        Ok(account)
    }

    /// Forgets the server: deletes `backup-server.json` and `project-sync-server.json`. Nothing on
    /// the server is touched.
    pub fn server_forget(&self) -> Result<()> {
        self.server_store.delete()?;
        remove_state(&self.project_state_path(Destination::Server))
    }
}
