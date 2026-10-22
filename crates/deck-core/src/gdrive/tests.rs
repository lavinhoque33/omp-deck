use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rusqlite::Connection;

use super::archive::Snapshot;
use super::*;

fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap()
}

/// A history db in WAL mode with `prompts` rows (as omp creates it).
fn history_db(path: &Path, prompts: &[&str]) {
    let c = Connection::open(path).unwrap();
    c.query_row("PRAGMA journal_mode=WAL", [], |_| Ok(()))
        .unwrap();
    c.execute_batch(
        "CREATE TABLE IF NOT EXISTS history (id INTEGER PRIMARY KEY AUTOINCREMENT, \
         prompt TEXT NOT NULL UNIQUE, created_at INTEGER NOT NULL DEFAULT 0)",
    )
    .unwrap();
    for p in prompts {
        c.execute("INSERT INTO history (prompt) VALUES (?1)", [p])
            .unwrap();
    }
}

fn prompts(c: &Connection) -> Vec<String> {
    c.prepare("SELECT prompt FROM history ORDER BY id")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

/// An agent dir with synced config, prompt history and things that must never be synced.
fn sample_agent(dir: &Path) {
    write(&dir.join("config.yml"), "theme: dark\n");
    write(&dir.join("models.yml"), "providers: {}\n");
    write(&dir.join("agents/reviewer.md"), "# reviewer\n");
    write(&dir.join("agents/nested/helper.md"), "# helper\n");
    write(&dir.join("skills/x/node_modules/dep.js"), "dep\n");
    write(&dir.join("agent.db"), "logins");
    write(&dir.join("sessions/s/1.jsonl"), "{}\n");
    write(&dir.join("config.yml.broken-123"), "bad");
    history_db(&dir.join(HISTORY_DB), &["first prompt", "second prompt"]);
}

fn entries(archive: &[u8]) -> Vec<String> {
    let mut a = tar::Archive::new(flate2::read::GzDecoder::new(archive));
    a.entries()
        .unwrap()
        .map(|e| e.unwrap().path().unwrap().display().to_string())
        .collect()
}

#[test]
fn backup_then_restore_brings_back_files_dirs_and_live_history() {
    let tmp = tempfile::tempdir().unwrap();
    let agent = tmp.path().join("agent");
    let safety = tmp.path().join("safety");
    sample_agent(&agent);
    let snap = Snapshot::take(&agent).unwrap();
    let bytes = snap.archive(1_792_000_000, "host-a").unwrap();
    let mut names = entries(&bytes);
    names.sort();
    assert_eq!(
        names,
        [
            "agent/agents/nested/helper.md",
            "agent/agents/reviewer.md",
            "agent/config.yml",
            "agent/history.db",
            "agent/models.yml",
            "manifest.json",
        ]
    );

    // Local drift after the backup.
    write(&agent.join("config.yml"), "theme: light\n");
    fs::remove_file(agent.join("agents/reviewer.md")).unwrap();
    write(&agent.join("agents/extra.md"), "# local only\n");
    write(&agent.join("agents/deep/only.md"), "# local only\n");
    write(&agent.join("SYSTEM.md"), "untouched: not in the backup\n");
    // omp keeps the db open in WAL mode while the restore runs.
    let omp = Connection::open(agent.join(HISTORY_DB)).unwrap();
    omp.execute(
        "INSERT INTO history (prompt) VALUES ('newer local prompt')",
        [],
    )
    .unwrap();
    omp.execute("DELETE FROM history WHERE prompt = 'first prompt'", [])
        .unwrap();

    let prepared =
        PreparedRestore::new(&bytes, &agent, &safety, "b.tar.gz".into(), 1_792_000_100).unwrap();
    assert_eq!(prepared.manifest.host, "host-a");
    assert_eq!(
        prepared.plan,
        RestorePlan {
            replaced: vec![
                "config.yml".into(),
                "history.db".into(),
                "models.yml".into()
            ],
            added: vec![],
            dirs: vec!["agents".into(), "skills".into()],
            removed: vec!["agents/deep/only.md".into(), "agents/extra.md".into()],
        }
    );
    assert_eq!(prepared.safety_dir, safety.join("20261014T174820Z"));
    let report = prepared.apply(&agent).unwrap();

    assert_eq!(read(&agent.join("config.yml")), "theme: dark\n");
    assert_eq!(read(&agent.join("agents/reviewer.md")), "# reviewer\n");
    assert_eq!(read(&agent.join("agents/nested/helper.md")), "# helper\n");
    assert!(!agent.join("agents/extra.md").exists());
    assert!(!agent.join("agents/deep").exists());
    assert_eq!(
        read(&agent.join("SYSTEM.md")),
        "untouched: not in the backup\n"
    );
    assert_eq!(read(&agent.join("agent.db")), "logins");
    assert_eq!(read(&agent.join("skills/x/node_modules/dep.js")), "dep\n");
    // The open connection sees the restored rows; the db is still in WAL mode.
    assert_eq!(prompts(&omp), ["first prompt", "second prompt"]);
    let mode: String = omp
        .query_row("PRAGMA journal_mode", [], |r| r.get(0))
        .unwrap();
    assert_eq!(mode, "wal");

    // The safety copy holds what was replaced or removed.
    let s = &report.safety_dir;
    assert_eq!(read(&s.join("config.yml")), "theme: light\n");
    assert_eq!(read(&s.join("agents/extra.md")), "# local only\n");
    assert_eq!(read(&s.join("agents/deep/only.md")), "# local only\n");
    let saved = Connection::open(s.join(HISTORY_DB)).unwrap();
    assert_eq!(prompts(&saved), ["second prompt", "newer local prompt"]);

    // Apart from the file the backup did not have, the content is exactly what was backed up.
    fs::remove_file(agent.join("SYSTEM.md")).unwrap();
    assert_eq!(
        Snapshot::take(&agent).unwrap().fingerprint(),
        prepared.manifest.fingerprint
    );
}

#[test]
fn fingerprint_changes_with_content_only() {
    let tmp = tempfile::tempdir().unwrap();
    let agent = tmp.path();
    sample_agent(agent);
    let fp = || Snapshot::take(agent).unwrap().fingerprint().to_owned();
    let first = fp();
    assert_eq!(fp(), first);
    write(&agent.join("sessions/s/2.jsonl"), "{}\n");
    write(&agent.join("agent.db"), "other logins");
    assert_eq!(fp(), first, "unsynced files do not count");
    history_db(&agent.join(HISTORY_DB), &["third prompt"]);
    let with_prompt = fp();
    assert_ne!(with_prompt, first);
    write(&agent.join("agents/reviewer.md"), "# changed\n");
    assert_ne!(fp(), with_prompt);
}

#[test]
fn safety_copies_keep_the_newest_five() {
    let tmp = tempfile::tempdir().unwrap();
    let (agent, safety) = (tmp.path().join("agent"), tmp.path().join("safety"));
    write(&agent.join("config.yml"), "a\n");
    let bytes = Snapshot::take(&agent).unwrap().archive(0, "h").unwrap();
    for i in 0..7 {
        PreparedRestore::new(&bytes, &agent, &safety, "b".into(), 1_000 + i)
            .unwrap()
            .apply(&agent)
            .unwrap();
    }
    let mut left: Vec<String> = fs::read_dir(&safety)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    left.sort();
    assert_eq!(
        left,
        [
            "19700101T001642Z",
            "19700101T001643Z",
            "19700101T001644Z",
            "19700101T001645Z",
            "19700101T001646Z"
        ]
    );
}

/// A gzipped tar with the given (raw path, entry type, data) entries plus a valid-looking
/// manifest listing nothing.
fn crafted(entries: &[(&str, tar::EntryType, &[u8])]) -> Vec<u8> {
    let mut b = tar::Builder::new(flate2::write::GzEncoder::new(
        Vec::new(),
        flate2::Compression::default(),
    ));
    for (path, kind, data) in entries {
        let mut h = tar::Header::new_old();
        // Raw name bytes: the builder's path helpers refuse `..`.
        h.as_old_mut().name[..path.len()].copy_from_slice(path.as_bytes());
        h.set_entry_type(*kind);
        h.set_size(data.len() as u64);
        h.set_mode(0o644);
        if *kind == tar::EntryType::Symlink {
            h.set_link_name("/etc/passwd").unwrap();
        }
        h.set_cksum();
        b.append(&h, *data).unwrap();
    }
    b.into_inner().unwrap().finish().unwrap()
}

#[test]
fn unsafe_archive_paths_are_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let agent = tmp.path().join("agent");
    let safety = tmp.path().join("safety");
    write(&agent.join("config.yml"), "mine\n");
    let file = tar::EntryType::Regular;
    let cases: &[(&str, tar::EntryType)] = &[
        ("../evil", file),
        ("/etc/evil", file),
        ("agent/../../evil", file),
        ("agent/./config.yml", file),
        ("agent/agent.db", file),
        ("agent/sessions/x.jsonl", file),
        ("agent/agents/link.md", tar::EntryType::Symlink),
        ("agent/agents/hard.md", tar::EntryType::Link),
    ];
    for (path, kind) in cases {
        let bytes = crafted(&[(path, *kind, b"x")]);
        let err = PreparedRestore::new(&bytes, &agent, &safety, "b".into(), 0)
            .err()
            .unwrap_or_else(|| panic!("{path} accepted"));
        assert!(matches!(err, Error::InvalidBackup(_)), "{path}: {err}");
    }
    // Nothing was touched.
    assert_eq!(read(&agent.join("config.yml")), "mine\n");
    assert!(!safety.exists());
    assert!(!tmp.path().join("evil").exists());
}

#[test]
fn tampered_archives_are_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let agent = tmp.path().join("agent");
    write(&agent.join("config.yml"), "a\n");
    let good = Snapshot::take(&agent).unwrap().archive(0, "h").unwrap();
    // Re-pack with one byte of config.yml changed: the checksum no longer matches.
    let mut a = tar::Archive::new(flate2::read::GzDecoder::new(&good[..]));
    let mut parts = Vec::new();
    for e in a.entries().unwrap() {
        let mut e = e.unwrap();
        let path = e.path().unwrap().display().to_string();
        let mut data = Vec::new();
        e.read_to_end(&mut data).unwrap();
        if path == "agent/config.yml" {
            data[0] = b'b';
        }
        parts.push((path, data));
    }
    let refs: Vec<(&str, tar::EntryType, &[u8])> = parts
        .iter()
        .map(|(p, d)| (p.as_str(), tar::EntryType::Regular, d.as_slice()))
        .collect();
    let err = PreparedRestore::new(&crafted(&refs), &agent, tmp.path(), "b".into(), 0)
        .err()
        .unwrap();
    assert!(err.to_string().contains("checksum"), "{err}");
}

fn file(id: &str, created: &str) -> BackupFile {
    BackupFile {
        id: id.into(),
        name: format!("{BACKUP_PREFIX}{id}.tar.gz"),
        size: 1,
        created: created.into(),
        props: BTreeMap::new(),
    }
}

#[test]
fn retention_deletes_all_but_the_newest() {
    let mut files: Vec<BackupFile> = (0..33)
        .map(|i| {
            file(
                &format!("f{i:02}"),
                &format!("2026-10-{:02}T10:00:00.000Z", i % 28 + 1),
            )
        })
        .collect();
    files.swap(0, 20);
    let doomed: Vec<&str> = to_delete(&files, KEEP_BACKUPS)
        .iter()
        .map(|f| f.id.as_str())
        .collect();
    // Days 1, 1 (f00 / f28), 2 (f01 / f29) are the oldest; ties fall back to the name.
    assert_eq!(doomed, ["f01", "f28", "f00"]);
    assert!(to_delete(&files[..5], KEEP_BACKUPS).is_empty());
}

#[test]
fn credentials_prefer_runtime_values_and_need_a_client_id() {
    let c = Credentials::pick(Some("rt".into()), Some("baked"), None, Some("s")).unwrap();
    assert_eq!(
        (c.client_id.as_str(), c.client_secret.as_deref()),
        ("rt", Some("s"))
    );
    let c = Credentials::pick(Some("  ".into()), Some("baked"), Some("x".into()), None).unwrap();
    assert_eq!(
        (c.client_id.as_str(), c.client_secret.as_deref()),
        ("baked", Some("x"))
    );
    assert!(Credentials::pick(None, None, Some("x".into()), Some("s")).is_none());
    assert!(Credentials::pick(None, Some(""), None, None).is_none());
}

#[test]
fn account_file_is_private() {
    let tmp = tempfile::tempdir().unwrap();
    let store = AccountStore::at(tmp.path().join("cfg/google-drive.json"));
    assert!(store.load().unwrap().is_none());
    let account = Account {
        refresh_token: "r".into(),
        email: Some("me@example.com".into()),
        ..Account::default()
    };
    store.save(&account).unwrap();
    let mode = fs::metadata(tmp.path().join("cfg/google-drive.json"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
    assert!(store.load().unwrap() == Some(account));
    store.delete().unwrap();
    assert!(store.load().unwrap().is_none());
}

// ---------------------------------------------------------------- OAuth loopback

fn creds() -> Credentials {
    Credentials {
        client_id: "cid.apps.googleusercontent.com".into(),
        client_secret: Some("sec".into()),
    }
}

fn endpoints(base: &str) -> Endpoints {
    Endpoints {
        auth: format!("{base}/auth"),
        token: format!("{base}/token"),
        revoke: format!("{base}/revoke"),
        drive: format!("{base}/drive/v3"),
        upload: format!("{base}/upload/drive/v3"),
    }
}

fn url_param(url: &str, name: &str) -> String {
    let query = url.split_once('?').unwrap().1;
    query
        .split('&')
        .find_map(|kv| kv.strip_prefix(&format!("{name}=")))
        .unwrap()
        .to_owned()
}

/// Sends a raw GET to the loopback; returns the whole response.
fn get(port: u16, target: &str) -> String {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    write!(s, "GET {target} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).unwrap();
    out
}

#[test]
fn consent_url_carries_pkce_state_and_loopback_redirect() {
    let flow = LoginFlow::start(&creds(), &endpoints("https://auth.test")).unwrap();
    let url = flow.url();
    assert!(url.starts_with("https://auth.test/auth?"));
    assert_eq!(
        url_param(url, "client_id"),
        "cid.apps.googleusercontent.com"
    );
    assert_eq!(
        url_param(url, "redirect_uri"),
        format!("http%3A%2F%2F127.0.0.1%3A{}", flow.port())
    );
    assert_eq!(
        url_param(url, "scope"),
        "https%3A%2F%2Fwww.googleapis.com%2Fauth%2Fdrive.appdata%20openid%20email"
    );
    assert_eq!(url_param(url, "code_challenge_method"), "S256");
    assert_eq!(url_param(url, "code_challenge").len(), 43);
    assert!(url_param(url, "state").len() >= 20);
}

#[test]
fn loopback_rejects_a_redirect_with_the_wrong_state() {
    let flow = LoginFlow::start(&creds(), &endpoints("https://auth.test")).unwrap();
    let port = flow.port();
    let waiter = std::thread::spawn(move || {
        flow.wait_code(&AtomicBool::new(false), Duration::from_secs(20))
    });
    assert!(get(port, "/favicon.ico").starts_with("HTTP/1.1 404"));
    let page = get(port, "/?state=forged&code=stolen");
    assert!(page.starts_with("HTTP/1.1 200"));
    assert!(page.contains("state mismatch"), "{page}");
    let err = waiter.join().unwrap().unwrap_err();
    assert!(
        matches!(&err, Error::Login(m) if m.contains("state mismatch")),
        "{err}"
    );
}

#[test]
fn loopback_returns_the_code_and_honours_cancel() {
    let flow = LoginFlow::start(&creds(), &endpoints("https://auth.test")).unwrap();
    let (port, state) = (flow.port(), url_param(flow.url(), "state"));
    let waiter = std::thread::spawn(move || {
        flow.wait_code(&AtomicBool::new(false), Duration::from_secs(20))
    });
    let page = get(port, &format!("/?state={state}&code=4%2Fabc&scope=x"));
    assert!(page.contains("You can close this tab"));
    assert_eq!(waiter.join().unwrap().unwrap(), "4/abc");

    let flow = LoginFlow::start(&creds(), &endpoints("https://auth.test")).unwrap();
    let cancel = AtomicBool::new(true);
    assert!(matches!(
        flow.wait_code(&cancel, Duration::from_secs(20)),
        Err(Error::Cancelled)
    ));
}

// ---------------------------------------------------------------- mock Google

#[derive(Default)]
struct Mock {
    /// Access tokens issued so far (`tok-1`, `tok-2`, …).
    issued: u32,
    /// The access token the mock treats as expired (answers 401 once per token).
    expired: Option<String>,
    files: BTreeMap<String, (BackupFile, Vec<u8>)>,
    next_id: u32,
    log: Vec<String>,
}

struct Request {
    method: String,
    path: String,
    query: BTreeMap<String, String>,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
}

fn read_request(stream: &TcpStream) -> Option<Request> {
    let mut r = BufReader::new(stream);
    let mut line = String::new();
    r.read_line(&mut line).ok()?;
    let mut parts = line.split_whitespace();
    let method = parts.next()?.to_owned();
    let target = parts.next()?.to_owned();
    let mut headers = BTreeMap::new();
    loop {
        let mut h = String::new();
        r.read_line(&mut h).ok()?;
        let h = h.trim_end();
        if h.is_empty() {
            break;
        }
        let (k, v) = h.split_once(':')?;
        headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_owned());
    }
    let len: usize = headers
        .get("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let mut body = vec![0; len];
    r.read_exact(&mut body).ok()?;
    let (path, q) = target.split_once('?').unwrap_or((&target, ""));
    let query = q
        .split('&')
        .filter(|kv| !kv.is_empty())
        .map(|kv| {
            let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
            (super::oauth::decode(k), super::oauth::decode(v))
        })
        .collect();
    Some(Request {
        method,
        path: path.to_owned(),
        query,
        headers,
        body,
    })
}

fn form(body: &[u8]) -> BTreeMap<String, String> {
    String::from_utf8_lossy(body)
        .split('&')
        .map(|kv| {
            let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
            (super::oauth::decode(k), super::oauth::decode(v))
        })
        .collect()
}

fn find(hay: &[u8], needle: &[u8]) -> usize {
    hay.windows(needle.len()).position(|w| w == needle).unwrap()
}

/// (metadata JSON, media bytes) of a `multipart/related` upload.
fn multipart(content_type: &str, body: &[u8]) -> (serde_json::Value, Vec<u8>) {
    let boundary = content_type.split("boundary=").nth(1).unwrap();
    let delim = format!("\r\n--{boundary}");
    let first = find(body, b"\r\n\r\n") + 4;
    let meta_end = first + find(&body[first..], delim.as_bytes());
    let meta = serde_json::from_slice(&body[first..meta_end]).unwrap();
    let rest = &body[meta_end + delim.len()..];
    let media_start = find(rest, b"\r\n\r\n") + 4;
    let media_end = media_start + find(&rest[media_start..], delim.as_bytes());
    (meta, rest[media_start..media_end].to_vec())
}

fn json(v: serde_json::Value) -> (u16, Vec<u8>) {
    (200, v.to_string().into_bytes())
}

fn meta_json(f: &BackupFile) -> serde_json::Value {
    serde_json::json!({
        "id": f.id, "name": f.name, "size": f.size.to_string(),
        "createdTime": f.created, "appProperties": f.props,
    })
}

fn route(mock: &mut Mock, req: &Request) -> (u16, Vec<u8>) {
    mock.log.push(format!("{} {}", req.method, req.path));
    match (req.method.as_str(), req.path.as_str()) {
        ("POST", "/token") => {
            let f = form(&req.body);
            assert_eq!(f["client_id"], "cid.apps.googleusercontent.com");
            assert_eq!(f["client_secret"], "sec");
            if f["refresh_token"] != "refresh-1" {
                return (400, br#"{"error":"invalid_grant"}"#.to_vec());
            }
            mock.issued += 1;
            json(
                serde_json::json!({"access_token": format!("tok-{}", mock.issued), "expires_in": 3599}),
            )
        }
        ("POST", "/revoke") => {
            assert_eq!(form(&req.body)["token"], "refresh-1");
            (200, b"{}".to_vec())
        }
        _ => {
            let auth = req
                .headers
                .get("authorization")
                .cloned()
                .unwrap_or_default();
            let token = auth.strip_prefix("Bearer ").unwrap_or_default().to_owned();
            if mock.expired.as_deref() == Some(token.as_str()) {
                mock.expired = None;
                return (
                    401,
                    br#"{"error":{"code":401,"message":"Invalid Credentials"}}"#.to_vec(),
                );
            }
            assert!(token.starts_with("tok-"), "unauthenticated {}", req.path);
            drive(mock, req)
        }
    }
}

fn drive(mock: &mut Mock, req: &Request) -> (u16, Vec<u8>) {
    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/drive/v3/files") => {
            assert_eq!(req.query["spaces"], "appDataFolder");
            let files: Vec<_> = mock.files.values().map(|(f, _)| meta_json(f)).collect();
            json(serde_json::json!({ "files": files }))
        }
        ("POST", "/upload/drive/v3/files") => {
            assert_eq!(req.query["uploadType"], "multipart");
            let (meta, media) = multipart(&req.headers["content-type"], &req.body);
            assert_eq!(meta["parents"], serde_json::json!(["appDataFolder"]));
            mock.next_id += 1;
            let f = BackupFile {
                id: format!("new{}", mock.next_id),
                name: meta["name"].as_str().unwrap().to_owned(),
                size: media.len() as u64,
                created: format!("2030-01-01T00:00:{:02}.000Z", mock.next_id),
                props: serde_json::from_value(meta["appProperties"].clone()).unwrap(),
            };
            let out = meta_json(&f);
            mock.files.insert(f.id.clone(), (f, media));
            json(out)
        }
        (method, path) => {
            let id = path.strip_prefix("/drive/v3/files/").unwrap_or_default();
            match (method, mock.files.get(id)) {
                ("GET", Some((_, data))) => {
                    assert_eq!(req.query["alt"], "media");
                    (200, data.clone())
                }
                ("DELETE", Some(_)) => {
                    mock.files.remove(id);
                    (204, Vec::new())
                }
                _ => (
                    404,
                    br#"{"error":{"code":404,"message":"File not found"}}"#.to_vec(),
                ),
            }
        }
    }
}

/// Serves `mock` on an ephemeral port; returns its base URL.
fn serve(mock: Arc<Mutex<Mock>>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let Some(req) = read_request(&stream) else {
                continue;
            };
            let (status, body) = route(&mut mock.lock().unwrap(), &req);
            let _ = write!(
                stream,
                "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
                 Connection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(&body);
        }
    });
    base
}

#[test]
fn backup_list_restore_and_prune_against_a_mock_google() {
    let tmp = tempfile::tempdir().unwrap();
    let agent = tmp.path().join("agent");
    sample_agent(&agent);
    let mut mock = Mock {
        // The first access token is already stale when Drive sees it.
        expired: Some("tok-1".into()),
        ..Mock::default()
    };
    for i in 0..31 {
        let f = file(
            &format!("old{i:02}"),
            &format!("2020-01-{:02}T00:00:00.000Z", i + 1),
        );
        mock.files.insert(f.id.clone(), (f, Vec::new()));
    }
    let mock = Arc::new(Mutex::new(mock));
    let service = Service {
        creds: creds(),
        endpoints: endpoints(&serve(mock.clone())),
        store: AccountStore::at(tmp.path().join("cfg/google-drive.json")),
        agent_dir: agent.clone(),
        safety_root: tmp.path().join("safety"),
    };
    assert!(matches!(service.backup(true), Err(Error::NotSignedIn)));
    service
        .store
        .save(&Account {
            refresh_token: "refresh-1".into(),
            email: Some("me@example.com".into()),
            ..Account::default()
        })
        .unwrap();

    let outcome = service
        .backup(false)
        .unwrap()
        .expect("first backup uploads");
    assert!(outcome.file.name.starts_with(BACKUP_PREFIX));
    assert!(
        outcome
            .file
            .name
            .ends_with(&format!("-{}.tar.gz", hostname()))
    );
    assert_eq!(outcome.file.host(), Some(hostname().as_str()));
    // 31 old + 1 new: the two oldest go.
    assert_eq!(outcome.pruned, 2);
    {
        let m = mock.lock().unwrap();
        assert!(!m.files.contains_key("old00") && !m.files.contains_key("old01"));
        assert!(m.files.contains_key("old02"));
        // Refresh → 401 on the stale token → refresh again → upload.
        assert_eq!(
            m.log[..4],
            [
                "POST /token",
                "POST /upload/drive/v3/files",
                "POST /token",
                "POST /upload/drive/v3/files"
            ]
        );
    }
    let account = service.store.load().unwrap().unwrap();
    assert_eq!(account.last_backup.unwrap().name, outcome.file.name);

    // Unchanged content: auto backup does nothing.
    assert!(service.backup(false).unwrap().is_none());

    let list = service.list().unwrap();
    assert_eq!(list.len(), KEEP_BACKUPS);
    assert_eq!(list[0].id, outcome.file.id, "newest first");

    write(&agent.join("config.yml"), "theme: light\n");
    fs::remove_file(agent.join("agents/reviewer.md")).unwrap();
    let prepared = service.prepare_restore(&list[0]).unwrap();
    assert_eq!(
        read(&agent.join("config.yml")),
        "theme: light\n",
        "prepare touches nothing"
    );
    service.apply_restore(&prepared).unwrap();
    assert_eq!(read(&agent.join("config.yml")), "theme: dark\n");
    assert_eq!(read(&agent.join("agents/reviewer.md")), "# reviewer\n");
    // The restored content is what Drive has: no re-upload.
    assert!(service.backup(false).unwrap().is_none());

    assert!(matches!(
        service.prepare_restore(&file("gone", "2020-01-01T00:00:00Z")),
        Err(Error::Google(m)) if m.contains("no longer exists")
    ));

    service.sign_out().unwrap();
    assert!(service.store.load().unwrap().is_none());
    assert!(
        mock.lock()
            .unwrap()
            .log
            .contains(&"POST /revoke".to_owned())
    );
}

#[test]
fn a_revoked_grant_signs_out() {
    let tmp = tempfile::tempdir().unwrap();
    let agent = tmp.path().join("agent");
    write(&agent.join("config.yml"), "a\n");
    let service = Service {
        creds: creds(),
        endpoints: endpoints(&serve(Arc::default())),
        store: AccountStore::at(tmp.path().join("google-drive.json")),
        agent_dir: agent,
        safety_root: tmp.path().join("safety"),
    };
    service
        .store
        .save(&Account {
            refresh_token: "revoked".into(),
            ..Account::default()
        })
        .unwrap();
    assert!(matches!(service.list(), Err(Error::SignedOut)));
    assert!(service.store.load().unwrap().is_none());
}
