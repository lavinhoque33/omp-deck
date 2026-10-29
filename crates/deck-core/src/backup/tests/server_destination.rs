//! The server destination against a real SFTP server in-process (russh server + russh-sftp over
//! a temp directory): trust on first use, the folder layout, and the shared `Service` logic.

use std::collections::{BTreeSet, HashMap};
use std::fs::OpenOptions;
use std::io::{Seek, SeekFrom};
use std::sync::atomic::Ordering;

use russh::keys::{PublicKey, decode_secret_key};
use russh::server::{Auth, ChannelOpenHandle, Msg, Session, VerifiedPublicKey};
use russh::{Channel, ChannelId};
use russh_sftp::protocol::{
    Attrs, Data, File as SftpFile, FileAttributes, Handle, Name, OpenFlags, Status, StatusCode,
    Version,
};

use super::*;
use crate::backup::remote::Remote;
use crate::backup::server::{
    Protocol, ServerAuth, ServerConfig, ServerStore, Sidecar, valid_id, valid_name,
};
use crate::sessions::SessionsRoot;

const HOST_KEY: &str = "-----BEGIN OPENSSH PRIVATE KEY-----
b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAMwAAAAtzc2gtZW
QyNTUxOQAAACDx9zzY5wOfwkelZndlui9hoKl/15u3fueKKYLhhQR+agAAAJikcTegpHE3
oAAAAAtzc2gtZWQyNTUxOQAAACDx9zzY5wOfwkelZndlui9hoKl/15u3fueKKYLhhQR+ag
AAAECQJknJo40x62oHZUywvMdcBQwIkpR7or19jAtAAaiGPfH3PNjnA5/CR6Vmd2W6L2Gg
qX/Xm7d+54opguGFBH5qAAAAEm9tcC1kZWNrLXRlc3QtaG9zdAECAw==
-----END OPENSSH PRIVATE KEY-----
";
/// `ssh-keygen -lf` of [`HOST_KEY`].
const HOST_FP: &str = "SHA256:RtCEJuIOIhTNnl4Cudle6JTJ7M5+IYBkVcNOgGUAQQc";
const OTHER_HOST_KEY: &str = "-----BEGIN OPENSSH PRIVATE KEY-----
b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAMwAAAAtzc2gtZW
QyNTUxOQAAACCFJVKQXCK0tT8FaC73Yy47hgFJRgLVy0mRniAiIXjBBwAAAJiYRf9XmEX/
VwAAAAtzc2gtZWQyNTUxOQAAACCFJVKQXCK0tT8FaC73Yy47hgFJRgLVy0mRniAiIXjBBw
AAAEAydU7dnQAlI14q4nM7veaWlVEcK67e51m73onW94hn5YUlUpBcIrS1PwVoLvdjLjuG
AUlGAtXLSZGeICIheMEHAAAAE29tcC1kZWNrLXRlc3Qtb3RoZXIBAg==
-----END OPENSSH PRIVATE KEY-----
";
const CLIENT_KEY: &str = "-----BEGIN OPENSSH PRIVATE KEY-----
b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAMwAAAAtzc2gtZW
QyNTUxOQAAACCHDvLEfu1d1vWXl8y0uF53x6WSuWwEQDCPNqUqUINUbwAAAJiH3Ap6h9wK
egAAAAtzc2gtZWQyNTUxOQAAACCHDvLEfu1d1vWXl8y0uF53x6WSuWwEQDCPNqUqUINUbw
AAAEAiBezyGC+5qc1Nz6xgAZhsZSjeoApVmmg5Boev7qE2XocO8sR+7V3W9ZeXzLS4XnfH
pZK5bARAMI82pSpQg1RvAAAAFG9tcC1kZWNrLXRlc3QtY2xpZW50AQ==
-----END OPENSSH PRIVATE KEY-----
";
const USER: &str = "deck";
const PASSWORD: &str = "s3cret";

// ---------------------------------------------------------------- the SFTP fixture

/// An SFTP server serving `root` as `/` on an ephemeral loopback port; runs for the rest of the
/// test process.
fn sftp_server(root: &Path, host_key: &str) -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    let config = Arc::new(russh::server::Config {
        keys: vec![decode_secret_key(host_key, None).unwrap()],
        auth_rejection_time: Duration::from_millis(1),
        auth_rejection_time_initial: Some(Duration::ZERO),
        ..russh::server::Config::default()
    });
    let client: PublicKey = decode_secret_key(CLIENT_KEY, None)
        .unwrap()
        .public_key()
        .clone();
    let root = root.to_owned();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async move {
            let listener = tokio::net::TcpListener::from_std(listener).unwrap();
            loop {
                let Ok((socket, _)) = listener.accept().await else {
                    return;
                };
                let ssh = Ssh {
                    root: root.clone(),
                    client: client.clone(),
                    channels: HashMap::new(),
                };
                let config = config.clone();
                tokio::spawn(async move {
                    if let Ok(session) = russh::server::run_stream(config, socket, ssh).await {
                        let _ = session.await;
                    }
                });
            }
        });
    });
    port
}

struct Ssh {
    root: PathBuf,
    client: PublicKey,
    channels: HashMap<ChannelId, Channel<Msg>>,
}

impl russh::server::Handler for Ssh {
    type Error = russh::Error;

    async fn auth_password(&mut self, user: &str, password: &str) -> Result<Auth, Self::Error> {
        Ok(if user == USER && password == PASSWORD {
            Auth::Accept
        } else {
            Auth::reject()
        })
    }

    async fn auth_publickey(
        &mut self,
        user: &str,
        key: VerifiedPublicKey<'_>,
    ) -> Result<Auth, Self::Error> {
        Ok(
            if user == USER && key.public_key.key_data() == self.client.key_data() {
                Auth::Accept
            } else {
                Auth::reject()
            },
        )
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        reply: ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.channels.insert(channel.id(), channel);
        reply.accept().await;
        Ok(())
    }

    async fn subsystem_request(
        &mut self,
        id: ChannelId,
        name: &str,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        match self.channels.remove(&id) {
            Some(channel) if name == "sftp" => {
                session.channel_success(id)?;
                let fs = FsSftp {
                    root: self.root.clone(),
                    handles: HashMap::new(),
                    next: 0,
                };
                russh_sftp::server::run(channel.into_stream(), fs).await;
            }
            _ => session.channel_failure(id)?,
        }
        Ok(())
    }
}

enum Open {
    File(fs::File),
    /// Entries not sent yet (`None` once sent).
    Dir(Option<Vec<SftpFile>>),
}

/// SFTP over a local directory: what the client in `server::sftp` uses, nothing more.
struct FsSftp {
    root: PathBuf,
    handles: HashMap<String, Open>,
    next: u64,
}

fn ok(id: u32) -> Status {
    Status {
        id,
        status_code: StatusCode::Ok,
        error_message: "Ok".into(),
        language_tag: "en-US".into(),
    }
}

fn attrs_of(meta: &fs::Metadata) -> FileAttributes {
    FileAttributes {
        size: Some(meta.len()),
        permissions: Some(if meta.is_dir() { 0o040_755 } else { 0o100_644 }),
        ..FileAttributes::empty()
    }
}

fn status_of(e: &std::io::Error) -> StatusCode {
    match e.kind() {
        std::io::ErrorKind::NotFound => StatusCode::NoSuchFile,
        std::io::ErrorKind::PermissionDenied => StatusCode::PermissionDenied,
        _ => StatusCode::Failure,
    }
}

impl FsSftp {
    fn local(&self, path: &str) -> PathBuf {
        self.root.join(path.trim_start_matches('/'))
    }

    fn handle(&mut self, open: Open) -> String {
        self.next += 1;
        let h = self.next.to_string();
        self.handles.insert(h.clone(), open);
        h
    }

    fn file(&mut self, handle: &str) -> Result<&mut fs::File, StatusCode> {
        match self.handles.get_mut(handle) {
            Some(Open::File(f)) => Ok(f),
            _ => Err(StatusCode::Failure),
        }
    }
}

impl russh_sftp::server::Handler for FsSftp {
    type Error = StatusCode;

    fn unimplemented(&self) -> Self::Error {
        StatusCode::OpUnsupported
    }

    async fn init(
        &mut self,
        _version: u32,
        _extensions: HashMap<String, String>,
    ) -> Result<Version, Self::Error> {
        Ok(Version::new())
    }

    async fn realpath(&mut self, id: u32, path: String) -> Result<Name, Self::Error> {
        let path = if path == "." || path.is_empty() {
            "/".to_owned()
        } else {
            path
        };
        Ok(Name {
            id,
            files: vec![SftpFile::dummy(path)],
        })
    }

    async fn stat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
        let meta = fs::metadata(self.local(&path)).map_err(|e| status_of(&e))?;
        Ok(Attrs {
            id,
            attrs: attrs_of(&meta),
        })
    }

    async fn lstat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
        self.stat(id, path).await
    }

    async fn mkdir(
        &mut self,
        id: u32,
        path: String,
        _attrs: FileAttributes,
    ) -> Result<Status, Self::Error> {
        fs::create_dir(self.local(&path)).map_err(|e| status_of(&e))?;
        Ok(ok(id))
    }

    async fn opendir(&mut self, id: u32, path: String) -> Result<Handle, Self::Error> {
        let mut files = Vec::new();
        for entry in fs::read_dir(self.local(&path)).map_err(|e| status_of(&e))? {
            let entry = entry.map_err(|e| status_of(&e))?;
            let meta = entry.metadata().map_err(|e| status_of(&e))?;
            files.push(SftpFile::new(
                entry.file_name().to_string_lossy(),
                attrs_of(&meta),
            ));
        }
        let handle = self.handle(Open::Dir(Some(files)));
        Ok(Handle { id, handle })
    }

    async fn readdir(&mut self, id: u32, handle: String) -> Result<Name, Self::Error> {
        match self.handles.get_mut(&handle) {
            Some(Open::Dir(files)) => match files.take() {
                Some(files) => Ok(Name { id, files }),
                None => Err(StatusCode::Eof),
            },
            _ => Err(StatusCode::Failure),
        }
    }

    async fn open(
        &mut self,
        id: u32,
        filename: String,
        pflags: OpenFlags,
        _attrs: FileAttributes,
    ) -> Result<Handle, Self::Error> {
        let options: OpenOptions = pflags.into();
        let file = options
            .open(self.local(&filename))
            .map_err(|e| status_of(&e))?;
        let handle = self.handle(Open::File(file));
        Ok(Handle { id, handle })
    }

    async fn read(
        &mut self,
        id: u32,
        handle: String,
        offset: u64,
        len: u32,
    ) -> Result<Data, Self::Error> {
        let file = self.file(&handle)?;
        file.seek(SeekFrom::Start(offset))
            .map_err(|e| status_of(&e))?;
        let mut data = vec![0u8; len as usize];
        let n = file.read(&mut data).map_err(|e| status_of(&e))?;
        if n == 0 {
            return Err(StatusCode::Eof);
        }
        data.truncate(n);
        Ok(Data { id, data })
    }

    async fn write(
        &mut self,
        id: u32,
        handle: String,
        offset: u64,
        data: Vec<u8>,
    ) -> Result<Status, Self::Error> {
        let file = self.file(&handle)?;
        file.seek(SeekFrom::Start(offset))
            .map_err(|e| status_of(&e))?;
        file.write_all(&data).map_err(|e| status_of(&e))?;
        Ok(ok(id))
    }

    async fn close(&mut self, id: u32, handle: String) -> Result<Status, Self::Error> {
        self.handles.remove(&handle);
        Ok(ok(id))
    }

    async fn remove(&mut self, id: u32, filename: String) -> Result<Status, Self::Error> {
        fs::remove_file(self.local(&filename)).map_err(|e| status_of(&e))?;
        Ok(ok(id))
    }

    async fn rename(
        &mut self,
        id: u32,
        oldpath: String,
        newpath: String,
    ) -> Result<Status, Self::Error> {
        let (from, to) = (self.local(&oldpath), self.local(&newpath));
        // SFTP v3: renaming onto an existing file fails.
        if to.exists() {
            return Err(StatusCode::Failure);
        }
        fs::rename(from, to).map_err(|e| status_of(&e))?;
        Ok(ok(id))
    }
}

// ---------------------------------------------------------------- helpers

/// A server destination over a fresh SFTP fixture; nothing is saved yet.
struct World {
    tmp: tempfile::TempDir,
    service: Service,
    /// The server's `/`.
    root: PathBuf,
    port: u16,
}

impl World {
    fn new() -> World {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("server");
        fs::create_dir_all(&root).unwrap();
        let port = sftp_server(&root, HOST_KEY);
        let agent = tmp.path().join("agent");
        sample_agent(&agent);
        let service = Service {
            creds: None,
            endpoints: endpoints("http://127.0.0.1:9"),
            store: AccountStore::at(tmp.path().join("cfg/google-drive.json")),
            server_store: ServerStore::at(tmp.path().join("cfg/backup-server.json")),
            destination: Destination::Server,
            agent_dir: agent,
            safety_root: tmp.path().join("safety"),
            cache_root: tmp.path().join("cache"),
        };
        World {
            tmp,
            service,
            root,
            port,
        }
    }

    /// Password login on the fixture, folder `backups`, host key pinned.
    fn config(&self) -> ServerConfig {
        ServerConfig {
            protocol: Protocol::Sftp,
            host: "127.0.0.1".into(),
            port: self.port,
            auth: ServerAuth::Password {
                user: USER.into(),
                password: PASSWORD.into(),
            },
            folder: "backups".into(),
            pinned: Some(HOST_FP.into()),
        }
    }

    /// Connected with [`Self::config`].
    fn connected() -> World {
        let w = World::new();
        w.service.server_connect(w.config()).unwrap();
        w
    }

    fn folder(&self) -> PathBuf {
        self.root.join("backups")
    }

    /// File names in the backup folder.
    fn names(&self) -> BTreeSet<String> {
        fs::read_dir(self.folder())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect()
    }

    /// Puts a complete backup (archive + sidecar) straight into the folder.
    fn seed(&self, name: &str, created: &str, props: &[(&str, &str)], data: &[u8]) {
        fs::create_dir_all(self.folder()).unwrap();
        fs::write(self.folder().join(name), data).unwrap();
        let sidecar = Sidecar {
            created: created.into(),
            size: data.len() as u64,
            props: props
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
        };
        fs::write(
            self.folder().join(format!("{name}.json")),
            serde_json::to_vec(&sidecar).unwrap(),
        )
        .unwrap();
    }
}

fn no_progress(_: Progress) {}

const OMP_ONLY: Parts = Parts {
    omp: true,
    sessions: false,
    memory: false,
};

// ---------------------------------------------------------------- trust and connect

#[test]
fn an_unknown_host_key_is_offered_for_trust_then_pinned_and_a_changed_one_refused() {
    let w = World::new();
    let unpinned = ServerConfig {
        pinned: None,
        ..w.config()
    };
    let err = w.service.server_connect(unpinned.clone()).unwrap_err();
    match &err {
        Error::UntrustedServer { fingerprint } => assert_eq!(fingerprint, HOST_FP),
        other => panic!("{other}"),
    }
    assert!(
        w.service.server_account().unwrap().is_none(),
        "nothing saved"
    );

    let account = w
        .service
        .server_connect(ServerConfig {
            pinned: Some(HOST_FP.into()),
            ..unpinned
        })
        .unwrap();
    assert_eq!(account.config.pinned.as_deref(), Some(HOST_FP));
    assert_eq!(w.service.server_account().unwrap(), Some(account));
    assert!(w.folder().is_dir(), "the folder is created");
    assert!(w.names().is_empty(), "the probe is removed");

    // Another machine answers on the pinned address.
    let tmp = tempfile::tempdir().unwrap();
    let impostor = sftp_server(tmp.path(), OTHER_HOST_KEY);
    let err = w
        .service
        .server_connect(ServerConfig {
            port: impostor,
            ..w.config()
        })
        .unwrap_err();
    assert!(
        matches!(&err, Error::ServerChanged { pinned, presented }
            if pinned == HOST_FP && presented.starts_with("SHA256:") && presented != HOST_FP),
        "{err}"
    );
}

#[cfg(unix)]
#[test]
fn server_file_is_private_and_never_logs_the_password() {
    let w = World::connected();
    let path = w.tmp.path().join("cfg/backup-server.json");
    let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    let account = w.service.server_account().unwrap().unwrap();
    assert!(!format!("{account:?}").contains(PASSWORD));
}

#[test]
fn wrong_password_and_key_login() {
    let w = World::new();
    let err = w
        .service
        .server_connect(ServerConfig {
            auth: ServerAuth::Password {
                user: USER.into(),
                password: "wrong".into(),
            },
            ..w.config()
        })
        .unwrap_err();
    assert!(matches!(err, Error::Server(_)), "{err}");
    assert!(w.service.server_account().unwrap().is_none());

    let key = w.tmp.path().join("id_ed25519");
    fs::write(&key, CLIENT_KEY).unwrap();
    w.service
        .server_connect(ServerConfig {
            auth: ServerAuth::Key {
                user: USER.into(),
                key_path: key,
                passphrase: None,
            },
            folder: "/deep/nested/dir".into(),
            ..w.config()
        })
        .unwrap();
    assert!(w.root.join("deep/nested/dir").is_dir());
}

#[test]
fn configs_are_normalized_and_impossible_combinations_refused() {
    let base = ServerConfig {
        protocol: Protocol::Ftp,
        host: "  example.org ".into(),
        port: 0,
        auth: ServerAuth::Anonymous,
        folder: " /srv//backups/ ".into(),
        pinned: Some(" ".into()),
    };
    let n = base.clone().normalized().unwrap();
    assert_eq!(
        (n.host.as_str(), n.port, n.folder.as_str(), n.pinned),
        ("example.org", 21, "/srv/backups", None)
    );
    let empty = ServerConfig {
        folder: String::new(),
        protocol: Protocol::Sftp,
        auth: ServerAuth::Password {
            user: "u".into(),
            password: "p".into(),
        },
        ..base.clone()
    };
    let n = empty.normalized().unwrap();
    assert_eq!((n.port, n.folder.as_str()), (22, server::DEFAULT_FOLDER));
    let refused = [
        ServerConfig {
            protocol: Protocol::Sftp,
            ..base.clone()
        },
        ServerConfig {
            auth: ServerAuth::Key {
                user: "u".into(),
                key_path: "k".into(),
                passphrase: None,
            },
            ..base.clone()
        },
        ServerConfig {
            folder: "a/../b".into(),
            ..base.clone()
        },
        ServerConfig {
            host: "  ".into(),
            ..base.clone()
        },
        ServerConfig {
            auth: ServerAuth::Password {
                user: " ".into(),
                password: "p".into(),
            },
            ..base
        },
    ];
    for c in refused {
        assert!(
            matches!(c.clone().normalized(), Err(Error::InvalidServer(_))),
            "{c:?}"
        );
    }
}

// ---------------------------------------------------------------- layout

#[test]
fn sidecars_parse_only_with_a_readable_creation_time() {
    let good = br#"{"created":"2026-10-22T13:52:01Z","size":12,"props":{"host":"h"}}"#;
    let s = Sidecar::parse(good).unwrap();
    assert_eq!((s.size, s.props["host"].as_str()), (12, "h"));
    assert!(Sidecar::parse(br#"{"created":"2026-10-22T13:52:01Z","size":1}"#).is_some());
    for bad in [
        &br#"{"created":"yesterday","size":1,"props":{}}"#[..],
        br#"{"size":1,"props":{}}"#,
        br#"{"created":"2026-10-22T13:52:01Z","size":-1}"#,
        b"not json",
    ] {
        assert!(
            Sidecar::parse(bad).is_none(),
            "{}",
            String::from_utf8_lossy(bad)
        );
    }
}

#[test]
fn only_plain_backup_names_are_valid_ids() {
    for ok in [
        "omp-backup-20261022T135201Z-host.tar.gz",
        "omp-project-20261022T135201Z-host-proj.tar.gz",
    ] {
        assert!(valid_id(ok), "{ok}");
    }
    for bad in [
        "",
        "../omp-backup-x.tar.gz",
        "omp-backup-a/b.tar.gz",
        "omp-backup-a\\b.tar.gz",
        "omp-backup-a..b.tar.gz",
        "omp-backup-a\nb.tar.gz",
        "omp-backup-x.tar.gz.json",
        "omp-backup-x.tar.gz.part",
        "notes.tar.gz",
    ] {
        assert!(!valid_id(bad), "{bad:?}");
    }
    assert!(valid_name("omp-deck-backups") && !valid_name("..") && !valid_name("a/b"));
    assert!(!valid_name(&"x".repeat(256)));
}

#[test]
fn uploads_land_as_archive_plus_sidecar_and_a_cancelled_one_leaves_nothing() {
    let w = World::connected();
    let mut remote = server::open(&w.config()).unwrap();
    let props = BTreeMap::from([("kind".to_owned(), "project".to_owned())]);
    let data = vec![7u8; 3 << 20];
    let path = w.tmp.path().join("big.bin");
    fs::write(&path, &data).unwrap();

    let cancel = AtomicBool::new(false);
    let err = remote
        .upload_file(
            "omp-project-x.tar.gz",
            &props,
            &mut fs::File::open(&path).unwrap(),
            data.len() as u64,
            &cancel,
            &mut |_| cancel.store(true, Ordering::Relaxed),
        )
        .unwrap_err();
    assert!(matches!(err, Error::Cancelled), "{err}");
    assert!(w.names().is_empty(), "{:?}", w.names());

    let mut seen = Vec::new();
    let file = remote
        .upload_file(
            "omp-project-x.tar.gz",
            &props,
            &mut fs::File::open(&path).unwrap(),
            data.len() as u64,
            &AtomicBool::new(false),
            &mut |n| seen.push(n),
        )
        .unwrap();
    assert_eq!(seen.last(), Some(&(data.len() as u64)));
    assert_eq!(
        (file.id.as_str(), file.size),
        ("omp-project-x.tar.gz", 3 << 20)
    );
    assert_eq!(
        w.names(),
        BTreeSet::from([
            "omp-project-x.tar.gz".to_owned(),
            "omp-project-x.tar.gz.json".to_owned()
        ])
    );
    let sidecar =
        Sidecar::parse(&fs::read(w.folder().join("omp-project-x.tar.gz.json")).unwrap()).unwrap();
    assert_eq!((sidecar.size, sidecar.props), (3 << 20, props));
    assert_eq!(
        fs::read(w.folder().join("omp-project-x.tar.gz")).unwrap(),
        data
    );
    assert_eq!(remote.metadata("omp-project-x.tar.gz").unwrap(), Some(file));

    assert!(matches!(
        remote.download("../etc/passwd"),
        Err(Error::Server(_))
    ));
    assert!(
        remote
            .metadata("omp-project-gone.tar.gz")
            .unwrap()
            .is_none()
    );
    remote
        .delete_permanently("omp-project-gone.tar.gz")
        .unwrap();
}

// ---------------------------------------------------------------- config backups

#[test]
fn backup_list_restore_and_prune_on_a_server() {
    let w = World::new();
    assert!(matches!(w.service.backup(true), Err(Error::NoServer)));
    w.service.server_connect(w.config()).unwrap();
    for i in 0..31 {
        w.seed(
            &format!("{BACKUP_PREFIX}old{i:02}.tar.gz"),
            &format!("2020-01-{:02}T00:00:00Z", i + 1),
            &[("host", "elsewhere"), ("fingerprint", "old")],
            b"x",
        );
    }
    // Incomplete (no sidecar), left over and foreign files are never listed or pruned.
    fs::write(w.folder().join(format!("{BACKUP_PREFIX}half.tar.gz")), b"x").unwrap();
    fs::write(
        w.folder().join(format!("{BACKUP_PREFIX}y.tar.gz.part")),
        b"x",
    )
    .unwrap();
    fs::write(w.folder().join("notes.txt"), b"mine").unwrap();

    let outcome = w
        .service
        .backup(false)
        .unwrap()
        .expect("first backup uploads");
    assert!(outcome.file.name.starts_with(BACKUP_PREFIX));
    assert_eq!(outcome.file.id, outcome.file.name);
    assert_eq!(outcome.file.host(), Some(hostname().as_str()));
    assert_eq!(outcome.pruned, 2, "31 old + 1 new: the two oldest go");
    let names = w.names();
    assert!(
        names.contains(&outcome.file.name)
            && names.contains(&format!("{}.json", outcome.file.name))
    );
    for gone in ["old00", "old01"] {
        assert!(!names.iter().any(|n| n.contains(gone)), "{gone}");
    }
    for kept in [
        "notes.txt",
        "omp-backup-half.tar.gz",
        "omp-backup-y.tar.gz.part",
    ] {
        assert!(names.contains(kept), "{kept}");
    }
    let account = w.service.server_account().unwrap().unwrap();
    assert_eq!(account.last_backup.unwrap().name, outcome.file.name);

    assert!(
        w.service.backup(false).unwrap().is_none(),
        "unchanged: skipped"
    );

    let list = w.service.list().unwrap();
    assert_eq!(list.len(), KEEP_BACKUPS);
    assert_eq!(list[0].id, outcome.file.id, "newest first");
    assert!(
        list.windows(2)
            .all(|p| p[0].created_unix() >= p[1].created_unix())
    );

    let agent = &w.service.agent_dir;
    write(&agent.join("config.yml"), "theme: light\n");
    let prepared = w.service.prepare_restore(&list[0]).unwrap();
    assert_eq!(
        read(&agent.join("config.yml")),
        "theme: light\n",
        "prepare touches nothing"
    );
    w.service.apply_restore(&prepared).unwrap();
    assert_eq!(read(&agent.join("config.yml")), "theme: dark\n");
    assert!(
        w.service.backup(false).unwrap().is_none(),
        "restored = synced"
    );

    let gone = BackupFile {
        id: format!("{BACKUP_PREFIX}gone.tar.gz"),
        name: format!("{BACKUP_PREFIX}gone.tar.gz"),
        ..list[0].clone()
    };
    assert!(matches!(
        w.service.prepare_restore(&gone),
        Err(Error::Server(_))
    ));
}

#[test]
fn deleting_on_a_server_checks_what_is_there_and_forgets_the_synced_state() {
    let w = World::connected();
    let first = w.service.backup(true).unwrap().unwrap().file;
    w.seed(
        &format!("{PROJECT_PREFIX}p.tar.gz"),
        "2030-01-01T00:00:00Z",
        &[("kind", "project"), ("host", "h"), ("pid", "p")],
        b"x",
    );

    // A project backup passed off as a config backup (no kind in the request) is refused.
    let disguised = BackupFile {
        id: format!("{PROJECT_PREFIX}p.tar.gz"),
        name: format!("{BACKUP_PREFIX}p.tar.gz"),
        ..first.clone()
    };
    assert!(matches!(
        w.service.delete_backup(&disguised),
        Err(Error::InvalidBackup(_))
    ));
    // The same id under another name.
    let renamed = BackupFile {
        name: format!("{BACKUP_PREFIX}other.tar.gz"),
        ..first.clone()
    };
    assert!(matches!(
        w.service.delete_backup(&renamed),
        Err(Error::InvalidBackup(_))
    ));
    assert_eq!(w.names().len(), 4);

    w.service.delete_backup(&first).unwrap();
    assert_eq!(w.names().len(), 2, "archive and sidecar gone");
    let account = w.service.server_account().unwrap().unwrap();
    assert!(account.last_backup.is_none() && account.synced_fingerprint.is_none());
    w.service.delete_backup(&first).unwrap();
    assert!(
        w.service.backup(false).unwrap().is_some(),
        "the next automatic pass uploads again"
    );
}

// ---------------------------------------------------------------- projects

#[test]
fn project_backup_restore_retention_and_delete_on_a_server() {
    let w = World::connected();
    let root = SessionsRoot::at(w.service.agent_dir.join("sessions"));
    let proj = w.tmp.path().join("proj");
    write(&proj.join(".omp/AGENTS.md"), "remote rules\n");
    let pid = path_id(&proj);
    let host = hostname();
    for i in 0..3 {
        w.seed(
            &format!("{PROJECT_PREFIX}old{i}.tar.gz"),
            &format!("2020-01-0{}T00:00:00Z", i + 1),
            &[("kind", "project"), ("host", &host), ("pid", &pid)],
            b"x",
        );
    }
    w.seed(
        &format!("{PROJECT_PREFIX}foreign.tar.gz"),
        "2019-01-01T00:00:00Z",
        &[("kind", "project"), ("host", "elsewhere"), ("pid", &pid)],
        b"x",
    );
    let backup = |force| {
        w.service.backup_project(
            &root,
            &proj,
            OMP_ONLY,
            &BTreeSet::new(),
            force,
            &AtomicBool::new(false),
            &no_progress,
        )
    };
    let out = backup(false).unwrap().expect("first backup uploads");
    assert_eq!(out.pruned, 1, "three kept per host and project");
    assert!(!w.names().iter().any(|n| n.contains("old0")));
    assert!(w.names().iter().any(|n| n.contains("foreign")));
    assert!(backup(false).unwrap().is_none(), "unchanged: skipped");

    let listed = w.service.list_projects().unwrap();
    assert_eq!(listed.len(), 4);
    assert_eq!(listed[0].file.name, out.backup.file.name, "newest first");
    assert_eq!(listed[0].path_id, pid);

    let target = w.tmp.path().join("target");
    let prepared = w
        .service
        .prepare_project_restore(
            &listed[0],
            &target,
            OMP_ONLY,
            &root,
            &AtomicBool::new(false),
            &no_progress,
        )
        .unwrap();
    assert!(!target.join(".omp").exists(), "prepare touches nothing");
    let report = w
        .service
        .apply_project_restore(
            prepared,
            &BTreeMap::new(),
            None,
            &AtomicBool::new(false),
            &no_progress,
        )
        .unwrap();
    assert!(!report.cancelled);
    assert_eq!(read(&target.join(".omp/AGENTS.md")), "remote rules\n");

    // Not a project backup: refused, nothing deleted.
    let mut config = listed[0].clone();
    config.file.props.remove("kind");
    assert!(matches!(
        w.service.delete_project_backup(&config),
        Err(Error::InvalidBackup(_))
    ));
    w.service.delete_project_backup(&listed[0]).unwrap();
    assert!(
        !w.names()
            .iter()
            .any(|n| n.starts_with(&out.backup.file.name))
    );
    assert!(
        !w.service.project_state().projects.contains_key(&proj),
        "the state of the deleted last backup is dropped"
    );
}

// ---------------------------------------------------------------- destinations

#[test]
fn each_destination_keeps_its_own_state() {
    let w = World::connected();
    let mock = Arc::new(Mutex::new(Mock::default()));
    let mut drive = drive_service(
        w.tmp.path(),
        "cfg/google-drive.json",
        &serve(mock.clone()),
        w.service.agent_dir.clone(),
    );
    drive.server_store = w.service.server_store.clone();
    drive
        .store
        .save(&Account {
            refresh_token: "refresh-1".into(),
            ..Account::default()
        })
        .unwrap();
    let server = drive.with_destination(Destination::Server);

    // Drive is synced; the server has never seen this content.
    drive.backup(false).unwrap().unwrap();
    assert!(drive.backup(false).unwrap().is_none());
    assert!(
        server.backup(false).unwrap().is_some(),
        "a switch is not up to date"
    );
    assert!(server.backup(false).unwrap().is_none());
    assert_eq!(server.list().unwrap().len(), 1);
    assert_eq!(drive.list().unwrap().len(), 1);

    let root = SessionsRoot::at(w.service.agent_dir.join("sessions"));
    let proj = w.tmp.path().join("proj");
    write(&proj.join(".omp/AGENTS.md"), "rules\n");
    server
        .backup_project(
            &root,
            &proj,
            OMP_ONLY,
            &BTreeSet::new(),
            false,
            &AtomicBool::new(false),
            &no_progress,
        )
        .unwrap()
        .unwrap();
    assert!(server.project_state().projects.contains_key(&proj));
    assert!(drive.project_state().projects.is_empty());
    let cfg = w.tmp.path().join("cfg");
    assert!(cfg.join("project-sync-server.json").is_file());
    assert!(!cfg.join("project-sync.json").exists());

    // Same server again (e.g. a new password): the state stays. Another folder: it starts over.
    server.server_connect(w.config()).unwrap();
    assert!(
        server
            .server_account()
            .unwrap()
            .unwrap()
            .synced_fingerprint
            .is_some()
    );
    assert!(server.project_state().projects.contains_key(&proj));
    server
        .server_connect(ServerConfig {
            folder: "elsewhere".into(),
            ..w.config()
        })
        .unwrap();
    assert!(
        server
            .server_account()
            .unwrap()
            .unwrap()
            .synced_fingerprint
            .is_none()
    );
    assert!(server.project_state().projects.is_empty());

    server.server_forget().unwrap();
    assert!(!cfg.join("backup-server.json").exists());
    assert!(!cfg.join("project-sync-server.json").exists());
    assert!(matches!(server.list(), Err(Error::NoServer)));
    assert!(
        cfg.join("google-drive.json").is_file(),
        "Drive is untouched"
    );
    assert!(drive.backup(false).unwrap().is_none());
}

#[test]
fn drive_is_unavailable_without_an_oauth_client() {
    let w = World::connected();
    let drive = w.service.with_destination(Destination::Drive);
    assert!(matches!(drive.list(), Err(Error::DriveUnavailable)));
    assert!(matches!(drive.begin_login(), Err(Error::DriveUnavailable)));
    assert!(
        w.service.list().unwrap().is_empty(),
        "the server still works"
    );
}
