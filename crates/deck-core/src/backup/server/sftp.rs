//! SFTP through `russh` (ring) + `russh-sftp`, driven from the blocking worker thread on a
//! private current-thread runtime. The host key is trusted on first use: an unpinned key is
//! [`Error::UntrustedServer`] with its OpenSSH `SHA256:` fingerprint; a key other than the pinned
//! one is [`Error::ServerChanged`].

use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use russh::client::{self, AuthResult, Handle};
use russh::keys::{HashAlg, PrivateKeyWithHashAlg, PublicKeyOrCertificate};
use russh::{ChannelId, Disconnect};
use russh_sftp::client::SftpSession;
use russh_sftp::client::error::Error as SftpError;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::runtime::Runtime;

use super::layout::Transport;
use super::{ServerAuth, ServerConfig};
use crate::backup::{CONNECT_TIMEOUT, Error, Result, STALL_TIMEOUT};

/// Transfer buffer.
const CHUNK: usize = 256 * 1024;

/// What the host key check saw.
#[derive(Debug)]
enum KeyCheck {
    Untrusted(String),
    Changed(String),
}

struct Client {
    pinned: Option<String>,
    seen: Arc<Mutex<Option<KeyCheck>>>,
}

/// `SHA256:<base64>`, as `ssh-keygen -l` prints it.
fn fingerprint(key: &PublicKeyOrCertificate) -> String {
    match key {
        PublicKeyOrCertificate::PublicKey { key, .. } => key.fingerprint(HashAlg::Sha256),
        PublicKeyOrCertificate::Certificate(cert) => cert.public_key().fingerprint(HashAlg::Sha256),
    }
    .to_string()
}

impl client::Handler for Client {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        key: &PublicKeyOrCertificate,
    ) -> std::result::Result<bool, Self::Error> {
        let fp = fingerprint(key);
        let verdict = match &self.pinned {
            Some(p) if *p == fp => return Ok(true),
            Some(_) => KeyCheck::Changed(fp),
            None => KeyCheck::Untrusted(fp),
        };
        *self.seen.lock().unwrap_or_else(|p| p.into_inner()) = Some(verdict);
        Ok(false)
    }

    async fn data(
        &mut self,
        _channel: ChannelId,
        _data: &[u8],
        _session: &mut client::Session,
    ) -> std::result::Result<(), Self::Error> {
        Ok(())
    }
}

struct Sftp {
    /// `None` only while dropping.
    sftp: Option<SftpSession>,
    ssh: Option<Handle<Client>>,
    /// The folder, absolute, without a trailing `/`.
    base: String,
    rt: Runtime,
}

fn failed(what: &str) -> impl FnOnce(SftpError) -> Error + '_ {
    move |e| Error::Server(format!("{what} failed: {e}"))
}

/// Connects, checks the host key, logs in, starts the SFTP subsystem and enters (creating) the
/// folder.
pub(super) fn connect(config: &ServerConfig) -> Result<Box<dyn Transport>> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| Error::Io(format!("cannot start the SFTP runtime: {e}")))?;
    let (ssh, sftp, base) = rt.block_on(session(config))?;
    Ok(Box::new(Sftp {
        sftp: Some(sftp),
        ssh: Some(ssh),
        base,
        rt,
    }))
}

async fn timed<T>(
    what: &str,
    f: impl Future<Output = std::result::Result<T, russh::Error>>,
) -> Result<T> {
    match tokio::time::timeout(CONNECT_TIMEOUT, f).await {
        Ok(r) => r.map_err(|e| Error::Server(format!("{what} failed: {e}"))),
        Err(_) => Err(Error::Server(format!("{what} timed out"))),
    }
}

async fn session(config: &ServerConfig) -> Result<(Handle<Client>, SftpSession, String)> {
    let seen = Arc::new(Mutex::new(None));
    let handler = Client {
        pinned: config.pinned.clone(),
        seen: seen.clone(),
    };
    let ssh_config = Arc::new(client::Config {
        inactivity_timeout: None,
        keepalive_interval: Some(Duration::from_secs(15)),
        keepalive_max: 4,
        ..client::Config::default()
    });
    let host = config.host.as_str();
    let port = config.effective_port();
    let connected = tokio::time::timeout(
        CONNECT_TIMEOUT,
        client::connect(ssh_config, (host, port), handler),
    )
    .await;
    let mut ssh = match connected {
        Err(_) => {
            return Err(Error::Server(format!(
                "connecting to {host}:{port} timed out"
            )));
        }
        Ok(Ok(h)) => h,
        Ok(Err(e)) => {
            let verdict = seen.lock().unwrap_or_else(|p| p.into_inner()).take();
            return Err(match verdict {
                Some(KeyCheck::Untrusted(fingerprint)) => Error::UntrustedServer { fingerprint },
                Some(KeyCheck::Changed(presented)) => Error::ServerChanged {
                    pinned: config.pinned.clone().unwrap_or_default(),
                    presented,
                },
                None => Error::Server(format!("cannot connect to {host}:{port}: {e}")),
            });
        }
    };
    let auth = match &config.auth {
        ServerAuth::Anonymous => {
            return Err(Error::InvalidServer(
                "SFTP needs a user name and a password or key".to_owned(),
            ));
        }
        ServerAuth::Password { user, password } => {
            timed("Logging in", ssh.authenticate_password(user, password)).await?
        }
        ServerAuth::Key {
            user,
            key_path,
            passphrase,
        } => {
            let key =
                russh::keys::load_secret_key(key_path, passphrase.as_deref()).map_err(|e| {
                    Error::InvalidServer(format!("cannot use the key {}: {e}", key_path.display()))
                })?;
            let hash = timed("Logging in", ssh.best_supported_rsa_hash())
                .await?
                .flatten();
            let key = PrivateKeyWithHashAlg::new(Arc::new(key), hash);
            timed("Logging in", ssh.authenticate_publickey(user, key)).await?
        }
    };
    if let AuthResult::Failure { .. } = auth {
        return Err(Error::Server(
            "the server refused the login (wrong user name, password or key)".to_owned(),
        ));
    }
    let channel = timed("Opening the SFTP session", ssh.channel_open_session()).await?;
    timed(
        "Opening the SFTP session",
        channel.request_subsystem(true, "sftp"),
    )
    .await?;
    let sftp = tokio::time::timeout(CONNECT_TIMEOUT, SftpSession::new(channel.into_stream()))
        .await
        .map_err(|_| Error::Server("Opening the SFTP session timed out".to_owned()))?
        .map_err(failed("Opening the SFTP session"))?;
    sftp.set_timeout(STALL_TIMEOUT.as_secs());
    let base = enter(&sftp, &config.folder).await?;
    Ok((ssh, sftp, base))
}

/// The absolute folder path, each missing level created.
async fn enter(sftp: &SftpSession, folder: &str) -> Result<String> {
    let mut path = if folder.starts_with('/') {
        String::new()
    } else {
        let home = sftp
            .canonicalize(".")
            .await
            .map_err(failed("Finding the login folder"))?;
        home.trim_end_matches('/').to_owned()
    };
    for part in folder.split('/').filter(|p| !p.is_empty()) {
        path = format!("{path}/{part}");
        let exists = sftp
            .try_exists(path.as_str())
            .await
            .map_err(failed(&format!("Opening the folder {path}")))?;
        if !exists {
            sftp.create_dir(path.as_str())
                .await
                .map_err(failed(&format!("Creating the folder {path}")))?;
        }
    }
    Ok(if path.is_empty() {
        "/".to_owned()
    } else {
        path
    })
}

impl Sftp {
    fn path(&self, name: &str) -> String {
        format!("{}/{name}", self.base.trim_end_matches('/'))
    }

    fn session(&self) -> &SftpSession {
        self.sftp.as_ref().expect("the session lives until drop")
    }
}

impl Transport for Sftp {
    fn list(&mut self) -> Result<Vec<String>> {
        let sftp = self.session();
        self.rt.block_on(async {
            let dir = sftp
                .read_dir(self.base.as_str())
                .await
                .map_err(failed("Listing the backups"))?;
            Ok(dir
                .map(|e| e.file_name())
                .filter(|n| n != "." && n != "..")
                .collect())
        })
    }

    fn read(&mut self, name: &str, out: &mut dyn Write) -> Result<()> {
        let path = self.path(name);
        let sftp = self.session();
        self.rt.block_on(async {
            let what = "Downloading the backup";
            let mut file = sftp.open(path.as_str()).await.map_err(failed(what))?;
            let mut buf = vec![0u8; CHUNK];
            loop {
                let n = file
                    .read(&mut buf)
                    .await
                    .map_err(|e| Error::Server(format!("{what} failed: {e}")))?;
                if n == 0 {
                    break;
                }
                out.write_all(&buf[..n])
                    .map_err(|e| Error::Server(format!("{what} failed: {e}")))?;
            }
            let _ = file.shutdown().await;
            out.flush()
                .map_err(|e| Error::Server(format!("{what} failed: {e}")))
        })
    }

    fn write(&mut self, name: &str, data: &mut dyn Read) -> Result<()> {
        let path = self.path(name);
        let sftp = self.session();
        self.rt.block_on(async {
            let what = "Uploading the backup";
            let mut file = sftp.create(path.as_str()).await.map_err(failed(what))?;
            let mut buf = vec![0u8; CHUNK];
            loop {
                let n = data
                    .read(&mut buf)
                    .map_err(|e| Error::Server(format!("{what} failed: {e}")))?;
                if n == 0 {
                    break;
                }
                file.write_all(&buf[..n])
                    .await
                    .map_err(|e| Error::Server(format!("{what} failed: {e}")))?;
            }
            file.shutdown()
                .await
                .map_err(|e| Error::Server(format!("{what} failed: {e}")))
        })
    }

    fn rename(&mut self, from: &str, to: &str) -> Result<()> {
        let (from, to) = (self.path(from), self.path(to));
        let sftp = self.session();
        self.rt
            .block_on(sftp.rename(from, to))
            .map_err(failed("Renaming the uploaded file"))
    }

    fn remove(&mut self, name: &str) -> Result<()> {
        let path = self.path(name);
        let sftp = self.session();
        self.rt
            .block_on(sftp.remove_file(path))
            .map_err(failed(&format!("Deleting {name}")))
    }
}

impl Drop for Sftp {
    fn drop(&mut self) {
        let (sftp, ssh) = (self.sftp.take(), self.ssh.take());
        self.rt.block_on(async {
            let bye = async {
                if let Some(s) = sftp {
                    let _ = s.close().await;
                }
                if let Some(h) = ssh {
                    let _ = h.disconnect(Disconnect::ByApplication, "", "en").await;
                }
            };
            let _ = tokio::time::timeout(Duration::from_secs(2), bye).await;
        });
    }
}
