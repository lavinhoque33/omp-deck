use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
#[cfg(unix)]
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

/// The gateway sends a [`BackupFile`] to its clients as JSON: what it serializes must decode,
/// besides Drive's own string-sized metadata.
#[test]
fn backup_file_survives_a_json_round_trip() {
    let mut f = file("a", "2026-10-01T10:00:00.000Z");
    f.size = 12_345;
    let back: BackupFile = serde_json::from_slice(&serde_json::to_vec(&f).unwrap()).unwrap();
    assert_eq!(back, f);
    let drive: BackupFile = serde_json::from_value(meta_json(&f)).unwrap();
    assert_eq!(drive, f);
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
#[cfg(unix)]
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

/// A scripted failure of the n-th data chunk of a resumable upload.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Fault {
    /// Drive stores the chunk but acknowledges 256 KiB less than was sent.
    ShortAck,
    /// Drive stores the chunk, then the connection drops without an answer.
    DropAfterStore,
    /// A 503 before anything is stored.
    Http503,
}

struct Upload {
    meta: serde_json::Value,
    total: u64,
    data: Vec<u8>,
}

#[derive(Default)]
struct Mock {
    /// Access tokens issued so far (`tok-1`, `tok-2`, …).
    issued: u32,
    /// The access token the mock treats as expired (answers 401 once per token).
    expired: Option<String>,
    files: BTreeMap<String, (BackupFile, Vec<u8>)>,
    next_id: u32,
    log: Vec<String>,
    uploads: BTreeMap<String, Upload>,
    next_upload: u32,
    /// Headers for the answer being built (`serve` sends and clears them).
    reply_headers: Vec<(String, String)>,
    /// Data chunks received so far (queries not counted).
    chunk_puts: u32,
    faults: BTreeMap<u32, Fault>,
    /// `Content-Range` of every upload PUT, chunks and `bytes */total` queries.
    range_log: Vec<String>,
    /// Each file download delivers only this many body bytes (front first), then the
    /// connection drops.
    cut_downloads: std::collections::VecDeque<usize>,
    /// Statuses answered to the next media GETs and session initiations (0 = connection drop).
    get_faults: std::collections::VecDeque<u16>,
    init_faults: std::collections::VecDeque<u16>,
    /// Every upload PUT is answered `308` without a `Range` (nothing is ever stored).
    always_308: bool,
    /// Downloads ignore `Range` and answer `200` with the whole file.
    ignore_range: bool,
    cut_body: Option<usize>,
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

/// Stores a finished upload as a new Drive file.
fn store_file(mock: &mut Mock, meta: &serde_json::Value, media: Vec<u8>) -> BackupFile {
    mock.next_id += 1;
    let f = BackupFile {
        id: format!("new{}", mock.next_id),
        name: meta["name"].as_str().unwrap().to_owned(),
        size: media.len() as u64,
        created: format!(
            "2030-01-01T{:02}:{:02}:{:02}.000Z",
            mock.next_id / 3600,
            mock.next_id / 60 % 60,
            mock.next_id % 60
        ),
        props: serde_json::from_value(meta["appProperties"].clone()).unwrap(),
    };
    mock.files.insert(f.id.clone(), (f.clone(), media));
    f
}

/// `Range: bytes=0-N` for a resumable upload that holds `len` bytes.
fn range_reply(mock: &mut Mock, len: usize) -> (u16, Vec<u8>) {
    if len > 0 {
        mock.reply_headers
            .push(("Range".into(), format!("bytes=0-{}", len - 1)));
    }
    (308, Vec::new())
}

fn resumable(mock: &mut Mock, req: &Request) -> (u16, Vec<u8>) {
    let id = req.query["upload_id"].clone();
    let Some(up) = mock.uploads.get(&id) else {
        return (404, br#"{"error":{"code":404,"message":"gone"}}"#.to_vec());
    };
    let total = up.total;
    let range = req.headers["content-range"].clone();
    mock.range_log.push(range.clone());
    if mock.always_308 {
        return (308, Vec::new());
    }
    let finish = |mock: &mut Mock| {
        let up = mock.uploads.remove(&id).unwrap();
        json(meta_json(&store_file(mock, &up.meta, up.data)))
    };
    if range.starts_with("bytes */") {
        assert_eq!(range, format!("bytes */{total}"));
        let len = up.data.len();
        return if len as u64 == total {
            finish(mock)
        } else {
            range_reply(mock, len)
        };
    }
    mock.chunk_puts += 1;
    let fault = mock.faults.remove(&mock.chunk_puts);
    if fault == Some(Fault::Http503) {
        return (503, br#"{"error":{"message":"backend error"}}"#.to_vec());
    }
    let (span, claimed) = range
        .strip_prefix("bytes ")
        .unwrap()
        .split_once('/')
        .unwrap();
    assert_eq!(claimed, total.to_string());
    let (from, to) = span.split_once('-').unwrap();
    let (from, to): (usize, usize) = (from.parse().unwrap(), to.parse().unwrap());
    assert_eq!(to + 1 - from, req.body.len());
    let up = mock.uploads.get_mut(&id).unwrap();
    if from != up.data.len() {
        return (
            400,
            format!("chunk starts at {from}, expected {}", up.data.len()).into_bytes(),
        );
    }
    up.data.extend_from_slice(&req.body);
    if fault == Some(Fault::DropAfterStore) {
        return (0, Vec::new());
    }
    if up.data.len() as u64 == total {
        return finish(mock);
    }
    if fault == Some(Fault::ShortAck) {
        let keep = up.data.len() - 256 * 1024;
        up.data.truncate(keep);
    }
    let len = up.data.len();
    range_reply(mock, len)
}

fn drive(mock: &mut Mock, req: &Request) -> (u16, Vec<u8>) {
    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/drive/v3/files") => {
            assert_eq!(req.query["spaces"], "appDataFolder");
            let files: Vec<_> = mock.files.values().map(|(f, _)| meta_json(f)).collect();
            json(serde_json::json!({ "files": files }))
        }
        ("POST", "/upload/drive/v3/files") if req.query["uploadType"] == "resumable" => {
            if let Some(status) = mock.init_faults.pop_front() {
                return (status, br#"{"error":{"message":"scripted"}}"#.to_vec());
            }
            let meta: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
            assert_eq!(meta["parents"], serde_json::json!(["appDataFolder"]));
            let total = req.headers["x-upload-content-length"].parse().unwrap();
            mock.next_upload += 1;
            let id = format!("up{}", mock.next_upload);
            mock.reply_headers.push((
                "Location".into(),
                format!(
                    "http://{}/upload/drive/v3/files?uploadType=resumable&upload_id={id}",
                    req.headers["host"]
                ),
            ));
            mock.uploads.insert(
                id,
                Upload {
                    meta,
                    total,
                    data: Vec::new(),
                },
            );
            (200, Vec::new())
        }
        ("PUT", "/upload/drive/v3/files") => resumable(mock, req),
        ("POST", "/upload/drive/v3/files") => {
            assert_eq!(req.query["uploadType"], "multipart");
            let (meta, media) = multipart(&req.headers["content-type"], &req.body);
            assert_eq!(meta["parents"], serde_json::json!(["appDataFolder"]));
            json(meta_json(&store_file(mock, &meta, media)))
        }
        (method, path) => {
            let id = path.strip_prefix("/drive/v3/files/").unwrap_or_default();
            match (method, mock.files.get(id)) {
                ("GET", Some((f, _))) if !req.query.contains_key("alt") => {
                    assert_eq!(
                        req.query["fields"],
                        "id,name,size,createdTime,appProperties"
                    );
                    json(meta_json(f))
                }
                ("GET", Some((_, data))) => {
                    assert_eq!(req.query["alt"], "media");
                    if let Some(status) = mock.get_faults.pop_front() {
                        return (status, br#"{"error":{"message":"scripted"}}"#.to_vec());
                    }
                    let data = data.clone();
                    if let Some(from) = req
                        .headers
                        .get("range")
                        .filter(|_| !mock.ignore_range)
                        .and_then(|r| r.strip_prefix("bytes="))
                        .and_then(|r| r.strip_suffix('-'))
                        .and_then(|r| r.parse::<usize>().ok())
                    {
                        mock.reply_headers.push((
                            "Content-Range".into(),
                            format!("bytes {from}-{}/{}", data.len() - 1, data.len()),
                        ));
                        mock.cut_body = mock.cut_downloads.pop_front();
                        return (206, data[from..].to_vec());
                    }
                    mock.cut_body = mock.cut_downloads.pop_front();
                    (200, data)
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
            let (status, body, headers, cut) = {
                let mut m = mock.lock().unwrap();
                let (status, body) = route(&mut m, &req);
                (
                    status,
                    body,
                    std::mem::take(&mut m.reply_headers),
                    m.cut_body.take(),
                )
            };
            if status == 0 {
                continue; // connection dropped without an answer
            }
            let extra: String = headers
                .iter()
                .map(|(k, v)| format!("{k}: {v}\r\n"))
                .collect();
            let _ = write!(
                stream,
                "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\n{extra}Content-Length: {}\r\n\
                 Connection: close\r\n\r\n",
                body.len()
            );
            let sent = cut.map_or(body.len(), |c| c.min(body.len()));
            let _ = stream.write_all(&body[..sent]);
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
        cache_root: tmp.path().join("cache"),
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
        cache_root: tmp.path().join("cache"),
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

/// A signed-in service over a fresh mock holding `files`.
fn signed_in_with(files: &[BackupFile]) -> (tempfile::TempDir, Service, Arc<Mutex<Mock>>) {
    let tmp = tempfile::tempdir().unwrap();
    let agent = tmp.path().join("agent");
    sample_agent(&agent);
    let mut mock = Mock::default();
    for f in files {
        mock.files.insert(f.id.clone(), (f.clone(), vec![1]));
    }
    let mock = Arc::new(Mutex::new(mock));
    let service = Service {
        creds: creds(),
        endpoints: endpoints(&serve(mock.clone())),
        store: AccountStore::at(tmp.path().join("cfg/google-drive.json")),
        agent_dir: agent,
        safety_root: tmp.path().join("safety"),
        cache_root: tmp.path().join("cache"),
    };
    service
        .store
        .save(&Account {
            refresh_token: "refresh-1".into(),
            ..Account::default()
        })
        .unwrap();
    (tmp, service, mock)
}

#[test]
fn deleting_a_config_backup_removes_it_and_404_counts_as_deleted() {
    let older = file("older", "2020-01-01T00:00:00.000Z");
    let (_tmp, service, mock) = signed_in_with(std::slice::from_ref(&older));
    let newest = service.backup(true).unwrap().unwrap().file;
    assert_eq!(service.list().unwrap().len(), 2);

    service.delete_backup(&older).unwrap();
    let left = service.list().unwrap();
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].id, newest.id);
    {
        let m = mock.lock().unwrap();
        assert!(m.log.contains(&"GET /drive/v3/files/older".to_owned()));
        assert!(m.log.contains(&"DELETE /drive/v3/files/older".to_owned()));
    }
    // An older backup leaves the recorded state alone: nothing is due.
    let account = service.store.load().unwrap().unwrap();
    assert_eq!(account.last_backup.unwrap().name, newest.name);
    assert!(service.backup(false).unwrap().is_none());

    // Already gone: success, nothing else is deleted.
    service.delete_backup(&older).unwrap();
    assert_eq!(service.list().unwrap().len(), 1);
    assert_eq!(
        mock.lock()
            .unwrap()
            .log
            .iter()
            .filter(|l| l.starts_with("DELETE"))
            .count(),
        1,
        "a file that is gone is not deleted again"
    );
}

#[test]
fn deleting_the_last_backup_makes_the_next_automatic_pass_upload_again() {
    let (_tmp, service, _mock) = signed_in_with(&[]);
    let first = service.backup(true).unwrap().unwrap().file;
    assert!(service.backup(false).unwrap().is_none());

    service.delete_backup(&first).unwrap();
    let account = service.store.load().unwrap().unwrap();
    assert!(
        account.last_backup.is_none(),
        "the deleted backup is no longer shown as the last one"
    );
    assert!(account.synced_fingerprint.is_none());
    assert!(service.list().unwrap().is_empty());
    let again = service.backup(false).unwrap();
    assert!(again.is_some(), "unchanged config is uploaded again");
}

#[test]
fn deleting_a_backup_of_the_synced_content_from_another_pc_forgets_the_sync() {
    let mut other = file("other", "2030-01-01T00:00:00.000Z");
    other
        .props
        .insert("fingerprint".into(), "restored-fp".into());
    let (_tmp, service, _mock) = signed_in_with(std::slice::from_ref(&other));
    let mut account = service.store.load().unwrap().unwrap();
    account.synced_fingerprint = Some("restored-fp".into());
    service.store.save(&account).unwrap();

    service.delete_backup(&other).unwrap();
    let account = service.store.load().unwrap().unwrap();
    assert!(account.synced_fingerprint.is_none());
    assert!(account.last_backup.is_none());
}

#[test]
fn only_config_backups_can_be_deleted() {
    let mut project = file("proj", "2030-01-01T00:00:00.000Z");
    project.name = format!("{PROJECT_PREFIX}proj.tar.gz");
    project.props = BTreeMap::from([
        ("kind".into(), "project".into()),
        ("host".into(), "h".into()),
    ]);
    let mut foreign = file("foreign", "2030-01-01T00:00:00.000Z");
    foreign.name = "notes.txt".into();
    // Named like a config backup but marked as something else.
    let mut marked = file("marked", "2030-01-01T00:00:00.000Z");
    marked.props.insert("kind".into(), "project".into());
    // A request that claims a config backup for an id holding a project backup.
    let mut disguised = project.clone();
    disguised.name = file("x", "2030-01-01T00:00:00.000Z").name;
    disguised.props.clear();

    let (_tmp, service, mock) = signed_in_with(&[project.clone(), foreign.clone(), marked.clone()]);
    for f in [&project, &foreign, &marked, &disguised] {
        let err = service.delete_backup(f).unwrap_err();
        assert!(matches!(err, Error::InvalidBackup(_)), "{}: {err}", f.name);
    }
    let m = mock.lock().unwrap();
    assert_eq!(m.files.len(), 3);
    assert!(!m.log.iter().any(|l| l.starts_with("DELETE")));
}

// ---------------------------------------------------------------- project sync

mod project_sync {
    use std::collections::BTreeSet;
    #[cfg(unix)]
    use std::os::unix::ffi::OsStrExt;
    #[cfg(unix)]
    use std::os::unix::fs::symlink;
    use std::path::PathBuf;

    use sha2::{Digest, Sha256};

    use super::*;
    use crate::gdrive::archive::hex;
    use crate::omp::Omp;
    use crate::sessions::{self, SessionsRoot};
    use crate::sharpshooter;

    const BOTH: Parts = Parts {
        omp: true,
        sessions: true,
        memory: false,
    };
    const OMP: Parts = Parts {
        omp: true,
        sessions: false,
        memory: false,
    };
    const SESSIONS: Parts = Parts {
        omp: false,
        sessions: true,
        memory: false,
    };
    const MEMORY: Parts = Parts {
        omp: false,
        sessions: false,
        memory: true,
    };
    const USER: &str = r#"{"type":"message","id":"m1","parentId":null,"timestamp":"2026-01-01T00:00:01.000Z","message":{"role":"user","content":"hi","timestamp":1}}"#;
    const ASSISTANT: &str = r#"{"type":"message","id":"m2","parentId":"m1","timestamp":"2026-01-01T00:00:02.000Z","message":{"role":"assistant","content":[{"type":"text","text":"hello"}],"api":"anthropic-messages","provider":"anthropic","model":"claude-sonnet-4-5","usage":{"input":1,"output":1,"cacheRead":0,"cacheWrite":0,"totalTokens":2,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}},"stopReason":"stop","timestamp":2}"#;

    struct World {
        tmp: tempfile::TempDir,
        service: Service,
        mock: Arc<Mutex<Mock>>,
        agent: PathBuf,
        root: SessionsRoot,
    }

    fn quiet(_: Progress) {}

    fn world_with(mock: Mock) -> World {
        let tmp = tempfile::tempdir().unwrap();
        let agent = tmp.path().join("agent");
        fs::create_dir_all(&agent).unwrap();
        let mock = Arc::new(Mutex::new(mock));
        let service = Service {
            creds: creds(),
            endpoints: endpoints(&serve(mock.clone())),
            store: AccountStore::at(tmp.path().join("cfg/google-drive.json")),
            agent_dir: agent.clone(),
            safety_root: tmp.path().join("safety"),
            cache_root: tmp.path().join("cache"),
        };
        service
            .store
            .save(&Account {
                refresh_token: "refresh-1".into(),
                ..Account::default()
            })
            .unwrap();
        World {
            root: SessionsRoot::at(agent.join("sessions")),
            tmp,
            service,
            mock,
            agent,
        }
    }

    fn world() -> World {
        world_with(Mock::default())
    }

    impl World {
        fn backup(
            &self,
            project: &Path,
            parts: Parts,
            force: bool,
        ) -> Result<Option<ProjectBackupOutcome>> {
            self.service.backup_project(
                &self.root,
                project,
                parts,
                &BTreeSet::new(),
                force,
                &AtomicBool::new(false),
                &quiet,
            )
        }

        fn prepare(
            &self,
            backup: &ProjectBackup,
            target: &Path,
            parts: Parts,
            root: &SessionsRoot,
        ) -> Result<PreparedProjectRestore> {
            self.service.prepare_project_restore(
                backup,
                target,
                parts,
                root,
                &AtomicBool::new(false),
                &quiet,
            )
        }

        /// Names of the staging directories left in the cache root.
        fn staging(&self) -> usize {
            fs::read_dir(self.tmp.path().join("cache")).map_or(0, |rd| rd.flatten().count())
        }
    }

    /// `<agent>/sessions/<enc>/<ts>_<id>.jsonl`: omp's layout, header on line 2 after a title.
    fn session(agent: &Path, id: &str, cwd: &Path, tail: &str) -> PathBuf {
        // omp's directory name for a cwd (session-paths.ts, `encodeLegacyAbsoluteSessionDirName`):
        // no leading separator, every `/`, `\\` and `:` becomes `-`, wrapped in `--`.
        let enc = format!(
            "--{}--",
            cwd.display()
                .to_string()
                .trim_start_matches(['/', '\\'])
                .replace(['/', '\\', ':'], "-")
        );
        let path = agent
            .join("sessions")
            .join(enc)
            .join(format!("2026-01-01T00-00-00-000Z_{id}.jsonl"));
        let header = format!(
            r#"{{"type":"session","version":3,"id":"{id}","timestamp":"2026-01-01T00:00:00.000Z","cwd":{}}}"#,
            serde_json::to_string(&cwd.display().to_string()).unwrap()
        );
        let title = r#"{"type":"title","v":1,"title":"t","updatedAt":"2026-01-01T00:00:00.000Z","pad":"   "}"#;
        write(
            &path,
            &format!("{title}\n{header}\n{USER}\n{ASSISTANT}\n{tail}"),
        );
        path
    }

    fn blob(agent: &Path, content: &str) -> String {
        let name = hex(&Sha256::digest(content.as_bytes()));
        write(&agent.join("blobs").join(&name), content);
        name
    }

    /// (path, data) of every member of a gzipped tar, in order.
    fn members(archive: &[u8]) -> Vec<(String, Vec<u8>)> {
        let mut a = tar::Archive::new(flate2::read::GzDecoder::new(archive));
        a.entries()
            .unwrap()
            .map(|e| {
                let mut e = e.unwrap();
                let path = e.path().unwrap().display().to_string();
                let mut data = Vec::new();
                e.read_to_end(&mut data).unwrap();
                (path, data)
            })
            .collect()
    }

    fn only_archive(w: &World) -> Vec<u8> {
        let m = w.mock.lock().unwrap();
        assert_eq!(m.files.len(), 1);
        m.files.values().next().unwrap().1.clone()
    }

    fn header_of(path: &Path) -> serde_json::Value {
        read(path)
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .find(|v| v["type"] == "session")
            .unwrap()
    }

    // ------------------------------------------------------------ .omp round trip

    #[test]
    fn omp_backup_round_trips_into_a_target_with_merge_and_safety_copy() {
        let w = world();
        let proj = w.tmp.path().join("proj");
        write(&proj.join(".omp/AGENTS.md"), "remote rules\n");
        write(&proj.join(".omp/config.yml"), "a: 1\n");
        write(&proj.join(".omp/hooks/pre/run.sh"), "#!/bin/sh\n");
        #[cfg(unix)]
        fs::set_permissions(
            proj.join(".omp/hooks/pre/run.sh"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        write(&proj.join(".omp/node_modules/dep/x.js"), "dep");
        write(&proj.join(".omp/.git/HEAD"), "ref");
        #[cfg(unix)]
        symlink(proj.join(".omp/AGENTS.md"), proj.join(".omp/link.md")).unwrap();

        let out = w
            .backup(&proj, OMP, false)
            .unwrap()
            .expect("first backup uploads");
        assert!(out.backup.file.name.starts_with(PROJECT_PREFIX));
        assert!(
            out.backup.file.name.ends_with("-proj.tar.gz"),
            "{}",
            out.backup.file.name
        );
        let b = &out.backup;
        assert_eq!(
            (b.host.as_str(), b.name.as_str(), b.path.as_str()),
            (hostname().as_str(), "proj", proj.to_str().unwrap())
        );
        assert_eq!(b.path_id, path_id(&proj));
        assert_eq!(b.omp_bytes, Some(13 + 5 + 10));
        assert_eq!((b.sessions, b.session_bytes), (None, None));
        assert_eq!(b.file.props["kind"], "project");
        assert_eq!(out.pruned, 0);
        assert_eq!(
            w.staging(),
            0,
            "the staging directory is removed after the job"
        );
        let names: Vec<String> = members(&only_archive(&w))
            .into_iter()
            .map(|m| m.0)
            .collect();
        assert_eq!(
            names,
            [
                "omp/AGENTS.md",
                "omp/config.yml",
                "omp/hooks/pre/run.sh",
                "manifest.json"
            ]
        );

        // Unchanged → skipped; forced → uploaded again.
        assert!(w.backup(&proj, OMP, false).unwrap().is_none());
        assert!(w.backup(&proj, OMP, true).unwrap().is_some());
        // Other parts are a different backup.
        assert!(matches!(
            w.backup(&proj, SESSIONS, false),
            Err(Error::NothingToBackUp(_))
        ));
        assert!(matches!(
            w.backup(&proj, Parts::default(), true),
            Err(Error::NothingToBackUp(_))
        ));
        let state_file = w.tmp.path().join("cfg/project-sync.json");
        #[cfg(unix)]
        assert_eq!(
            fs::metadata(&state_file).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(state_file.is_file());
        let state = w.service.project_state();
        assert_eq!(state.projects[&proj].parts, OMP);
        assert_eq!(state.projects[&proj].fingerprint, out.synced.fingerprint);

        let list = w.service.list_projects().unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].path_id, path_id(&proj));

        // The target has its own .omp: one file overlaps, one is local only.
        let target = w.tmp.path().join("target");
        write(&target.join(".omp/AGENTS.md"), "local edit\n");
        write(&target.join(".omp/local-only.md"), "mine\n");
        let prepared = w.prepare(&list[0], &target, OMP, &w.root).unwrap();
        assert_eq!(w.staging(), 1);
        let plan = &prepared.plan;
        assert!(plan.omp_exists && !plan.create_target);
        assert_eq!(plan.omp_overwritten, ["AGENTS.md"]);
        assert_eq!(plan.omp_added, ["config.yml", "hooks/pre/run.sh"]);
        assert!(plan.sessions.is_empty());
        assert_eq!(
            read(&target.join(".omp/AGENTS.md")),
            "local edit\n",
            "prepare touches nothing"
        );

        let report = w
            .service
            .apply_project_restore(
                prepared,
                &BTreeMap::new(),
                None,
                &AtomicBool::new(false),
                &quiet,
            )
            .unwrap();
        assert_eq!((report.omp_written, report.omp_overwritten), (3, 1));
        assert_eq!(read(&target.join(".omp/AGENTS.md")), "remote rules\n");
        assert_eq!(read(&target.join(".omp/config.yml")), "a: 1\n");
        assert_eq!(read(&target.join(".omp/local-only.md")), "mine\n");
        #[cfg(unix)]
        {
            let mode = fs::metadata(target.join(".omp/hooks/pre/run.sh"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o111, 0o111, "exec bit kept");
        }
        assert!(
            !target.join(".omp/node_modules").exists() && !target.join(".omp/link.md").exists()
        );
        let safety = report.safety_dir.expect("an overwrite makes a safety copy");
        assert!(safety.starts_with(w.tmp.path().join("safety")));
        assert!(
            safety
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("project-")
        );
        assert_eq!(read(&safety.join("AGENTS.md")), "local edit\n");
        assert_eq!(
            w.staging(),
            0,
            "the staging directory goes with the prepared restore"
        );

        // A target that does not exist is created; nothing to overwrite → no safety copy.
        let fresh = w.tmp.path().join("deep/new-target");
        let prepared = w.prepare(&list[0], &fresh, OMP, &w.root).unwrap();
        assert!(prepared.plan.create_target && !prepared.plan.omp_exists);
        assert_eq!(prepared.plan.omp_added.len(), 3);
        let report = w
            .service
            .apply_project_restore(
                prepared,
                &BTreeMap::new(),
                None,
                &AtomicBool::new(false),
                &quiet,
            )
            .unwrap();
        assert!(report.safety_dir.is_none());
        assert_eq!(read(&fresh.join(".omp/config.yml")), "a: 1\n");

        // A part the backup does not have, and `.omp` into $HOME, are refused.
        assert!(matches!(
            w.prepare(&list[0], &target, BOTH, &w.root),
            Err(Error::InvalidBackup(_))
        ));
        let home = dirs::home_dir().unwrap();
        assert!(matches!(
            w.prepare(&list[0], &home, OMP, &w.root),
            Err(Error::Io(_))
        ));
        assert_eq!(w.staging(), 0);
    }

    #[test]
    fn retention_keeps_the_newest_three_per_host_and_project_only() {
        let mut mock = Mock::default();
        let proj = Path::new("/work/proj");
        let other = |id: &str, host: &str, pid: &str| {
            let mut f = file(id, "2020-01-01T00:00:00.000Z");
            f.name = format!("{PROJECT_PREFIX}{id}.tar.gz");
            f.props = BTreeMap::from([
                ("kind".into(), "project".into()),
                ("host".into(), host.into()),
                ("pid".into(), pid.into()),
            ]);
            f
        };
        for i in 0..4 {
            for (host, pid) in [
                (hostname(), "otherproject".to_owned()),
                ("another-pc".to_owned(), path_id(proj)),
            ] {
                let f = other(&format!("keep-{host}-{pid}-{i}"), &host, &pid);
                mock.files.insert(f.id.clone(), (f, Vec::new()));
            }
        }
        let config_backup = file("config-backup", "2020-01-01T00:00:00.000Z");
        mock.files
            .insert(config_backup.id.clone(), (config_backup, Vec::new()));
        let w = world_with(mock);
        let dir = w.tmp.path().join("work/proj");
        write(&dir.join(".omp/AGENTS.md"), "x\n");
        // The backed-up path differs from `proj`: the identity is the path, so use its own id.
        let pid = path_id(&dir);
        let mut pruned = Vec::new();
        for _ in 0..5 {
            pruned.push(w.backup(&dir, OMP, true).unwrap().unwrap().pruned);
        }
        assert_eq!(pruned, [0, 0, 0, 1, 1]);
        let m = w.mock.lock().unwrap();
        let count =
            |pred: &dyn Fn(&BackupFile) -> bool| m.files.values().filter(|(f, _)| pred(f)).count();
        let host = hostname();
        assert_eq!(
            count(&|f| f.props.get("pid") == Some(&pid) && f.props.get("host") == Some(&host)),
            3
        );
        assert_eq!(
            count(&|f| f.props.get("pid").map(String::as_str) == Some("otherproject")),
            4
        );
        assert_eq!(
            count(&|f| f.props.get("host").map(String::as_str) == Some("another-pc")),
            4
        );
        assert!(m.files.contains_key("config-backup"));
        // The three left are the newest ones.
        let mut mine: Vec<&str> = m
            .files
            .values()
            .filter(|(f, _)| f.props.get("pid") == Some(&pid))
            .map(|(f, _)| f.id.as_str())
            .collect();
        mine.sort();
        assert_eq!(mine, ["new3", "new4", "new5"]);
    }

    // ------------------------------------------------------------ sessions snapshot

    #[test]
    fn live_session_is_cut_at_its_last_newline_and_only_referenced_blobs_travel() {
        let w = world();
        let proj = w.tmp.path().join("proj");
        fs::create_dir_all(&proj).unwrap();
        let referenced = blob(&w.agent, "referenced blob");
        let in_artifact = blob(&w.agent, "artifact blob");
        let unreferenced = blob(&w.agent, "nobody points here");
        let missing = "c".repeat(64);
        // The last line is half written: omp is appending while the backup reads.
        let main = session(
            &w.agent,
            "sess-1",
            &proj,
            &format!(
                "{{\"ref\":\"blob:sha256:{referenced}\"}}\n{{\"ref\":\"blob:sha256:{missing}\"}}\n{{\"type\":\"mess"
            ),
        );
        let stem = main.with_extension("");
        write(
            &stem.join("sub.jsonl"),
            &format!("{{\"r\":\"x blob:sha256:{in_artifact}\"}}\n"),
        );
        write(
            &stem.join("files/out.txt"),
            &format!("blob:sha256:{unreferenced}"),
        );

        let sizes = measure(&w.root, &proj).unwrap();
        assert_eq!(sizes.omp, OmpDir::Missing);
        assert_eq!(sizes.sessions, 1);

        let out = w.backup(&proj, SESSIONS, false).unwrap().unwrap();
        assert_eq!(out.backup.sessions, Some(1));
        let parts = members(&only_archive(&w));
        let names: BTreeSet<&str> = parts.iter().map(|m| m.0.as_str()).collect();
        let expected: BTreeSet<String> = [
            "sessions/sess-1/session.jsonl".to_owned(),
            "sessions/sess-1/artifacts/sub.jsonl".to_owned(),
            "sessions/sess-1/artifacts/files/out.txt".to_owned(),
            format!("blobs/{referenced}"),
            format!("blobs/{in_artifact}"),
            "manifest.json".to_owned(),
        ]
        .into();
        assert_eq!(names, expected.iter().map(String::as_str).collect());
        assert_eq!(
            parts.last().unwrap().0,
            "manifest.json",
            "the manifest is last"
        );
        let jsonl = &parts
            .iter()
            .find(|m| m.0.ends_with("session.jsonl"))
            .unwrap()
            .1;
        let text = String::from_utf8(jsonl.clone()).unwrap();
        assert!(text.ends_with("\"}\n"), "{text:?}");
        assert!(
            !text.contains("\"type\":\"mess\""),
            "the partial line is not archived"
        );
        assert_eq!(text.lines().count(), 6);
        assert_eq!(
            out.backup.session_bytes,
            Some(
                jsonl.len() as u64
                    + fs::metadata(stem.join("sub.jsonl")).unwrap().len()
                    + fs::metadata(stem.join("files/out.txt")).unwrap().len()
            )
        );

        // Unchanged → skipped. The session grows → a new backup with a new fingerprint.
        assert!(w.backup(&proj, SESSIONS, false).unwrap().is_none());
        std::thread::sleep(Duration::from_millis(20));
        let mut f = fs::OpenOptions::new().append(true).open(&main).unwrap();
        writeln!(f, "{{\"later\":true}}").unwrap();
        drop(f);
        let again = w.backup(&proj, SESSIONS, false).unwrap().unwrap();
        assert_ne!(again.synced.fingerprint, out.synced.fingerprint);
        assert!(w.backup(&proj, SESSIONS, false).unwrap().is_none());
    }

    #[test]
    fn other_projects_sessions_are_not_included() {
        let w = world();
        let (a, b) = (w.tmp.path().join("a"), w.tmp.path().join("b"));
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&b).unwrap();
        session(&w.agent, "in-a", &a, "");
        session(&w.agent, "in-b", &b, "");
        w.backup(&a, SESSIONS, false).unwrap().unwrap();
        let names: Vec<String> = members(&only_archive(&w))
            .into_iter()
            .map(|m| m.0)
            .collect();
        assert_eq!(names, ["sessions/in-a/session.jsonl", "manifest.json"]);
        assert_eq!(sessions::scan(&w.root).len(), 2);
    }

    // ------------------------------------------------------------ exclusions and deletion

    fn ids(items: &[&str]) -> BTreeSet<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    fn backup_without(
        w: &World,
        project: &Path,
        parts: Parts,
        excluded: &BTreeSet<String>,
        force: bool,
    ) -> Result<Option<ProjectBackupOutcome>> {
        w.service.backup_project(
            &w.root,
            project,
            parts,
            excluded,
            force,
            &AtomicBool::new(false),
            &quiet,
        )
    }

    fn session_dirs(archive: &[u8]) -> BTreeSet<String> {
        members(archive)
            .into_iter()
            .filter_map(|m| {
                m.0.strip_prefix("sessions/")
                    .and_then(|r| r.split('/').next())
                    .map(str::to_owned)
            })
            .collect()
    }

    #[test]
    fn excluded_sessions_stay_out_while_later_ones_are_included() {
        let w = world();
        let proj = w.tmp.path().join("proj");
        fs::create_dir_all(&proj).unwrap();
        session(&w.agent, "keep", &proj, "");
        session(&w.agent, "skip", &proj, "");
        let skip = ids(&["skip"]);

        let first = backup_without(&w, &proj, SESSIONS, &skip, false)
            .unwrap()
            .unwrap();
        assert_eq!(first.backup.sessions, Some(1));
        assert!(first.skipped.is_empty(), "{:?}", first.skipped);
        assert_eq!(session_dirs(&only_archive(&w)), ids(&["keep"]));
        assert!(
            backup_without(&w, &proj, SESSIONS, &skip, false)
                .unwrap()
                .is_none(),
            "same exclusions, nothing changed"
        );

        // A session created afterwards is not excluded: it is picked up automatically.
        session(&w.agent, "later", &proj, "");
        let second = backup_without(&w, &proj, SESSIONS, &skip, false)
            .unwrap()
            .unwrap();
        assert_eq!(second.backup.sessions, Some(2));
        let archive = {
            let m = w.mock.lock().unwrap();
            m.files["new2"].1.clone()
        };
        assert_eq!(session_dirs(&archive), ids(&["keep", "later"]));
    }

    #[test]
    fn changing_the_exclusions_counts_as_a_change() {
        let w = world();
        let proj = w.tmp.path().join("proj");
        fs::create_dir_all(&proj).unwrap();
        session(&w.agent, "a", &proj, "");
        session(&w.agent, "b", &proj, "");
        let none = BTreeSet::new();
        let first = backup_without(&w, &proj, SESSIONS, &none, false)
            .unwrap()
            .unwrap();
        assert_eq!(first.backup.sessions, Some(2));
        assert!(
            backup_without(&w, &proj, SESSIONS, &none, false)
                .unwrap()
                .is_none()
        );
        let narrowed = backup_without(&w, &proj, SESSIONS, &ids(&["a"]), false)
            .unwrap()
            .expect("a new exclusion is a change");
        assert_eq!(narrowed.backup.sessions, Some(1));
        assert_ne!(narrowed.synced.fingerprint, first.synced.fingerprint);
        let widened = backup_without(&w, &proj, SESSIONS, &none, false)
            .unwrap()
            .expect("lifting the exclusion is a change");
        assert_eq!(widened.backup.sessions, Some(2));
        assert_eq!(widened.synced.fingerprint, first.synced.fingerprint);
    }

    #[test]
    fn nothing_left_after_excluding_every_session_is_nothing_to_back_up() {
        let w = world();
        let proj = w.tmp.path().join("proj");
        fs::create_dir_all(&proj).unwrap();
        session(&w.agent, "a", &proj, "");
        let all = ids(&["a"]);
        let err = backup_without(&w, &proj, SESSIONS, &all, true).unwrap_err();
        assert!(matches!(err, Error::NothingToBackUp(_)), "{err}");
        assert!(w.mock.lock().unwrap().files.is_empty());

        // `.omp` still counts as something to back up.
        write(&proj.join(".omp/AGENTS.md"), "rules");
        let both = Parts {
            omp: true,
            sessions: true,
            memory: false,
        };
        let out = backup_without(&w, &proj, both, &all, true)
            .unwrap()
            .unwrap();
        assert_eq!(out.backup.sessions, None);
        assert!(out.backup.omp_bytes.is_some());
    }

    #[test]
    fn excluding_an_id_that_is_not_a_session_of_the_project_is_no_change() {
        let w = world();
        let proj = w.tmp.path().join("proj");
        fs::create_dir_all(&proj).unwrap();
        session(&w.agent, "a", &proj, "");
        let first = backup_without(&w, &proj, SESSIONS, &BTreeSet::new(), false)
            .unwrap()
            .unwrap();
        let stale = ids(&["deleted-long-ago"]);
        assert!(
            backup_without(&w, &proj, SESSIONS, &stale, false)
                .unwrap()
                .is_none(),
            "a stale exclusion changes nothing"
        );
        assert_eq!(
            w.service.project_state().projects[&proj].fingerprint,
            first.synced.fingerprint
        );
    }

    #[test]
    fn measure_lists_every_session_newest_first_with_title_and_size() {
        let w = world();
        let proj = w.tmp.path().join("proj");
        fs::create_dir_all(&proj).unwrap();
        let old = session(&w.agent, "old", &proj, "");
        let new = session(&w.agent, "new", &proj, "");
        let t = |secs| std::time::UNIX_EPOCH + Duration::from_secs(secs);
        fs::File::options()
            .write(true)
            .open(&old)
            .unwrap()
            .set_modified(t(1_000_000))
            .unwrap();
        fs::File::options()
            .write(true)
            .open(&new)
            .unwrap()
            .set_modified(t(2_000_000))
            .unwrap();
        write(&new.with_extension("").join("a.txt"), "12345");

        let sizes = measure(&w.root, &proj).unwrap();
        let listed: Vec<&str> = sizes.session_list.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(listed, ["new", "old"]);
        assert_eq!(sizes.sessions, 2);
        assert_eq!(
            sizes.session_bytes,
            sizes.session_list.iter().map(|s| s.bytes).sum::<u64>()
        );
        assert_eq!(
            sizes.session_list[0].bytes,
            fs::metadata(&new).unwrap().len() + 5
        );
        assert_eq!(sizes.session_list[0].title, "t");
        assert_eq!(sizes.session_list[0].modified, t(2_000_000));
    }

    #[test]
    fn deleting_a_backup_removes_it_from_drive_and_forgets_the_recorded_one() {
        let w = world();
        let proj = w.tmp.path().join("proj");
        fs::create_dir_all(&proj).unwrap();
        session(&w.agent, "s1", &proj, "");
        let out = w.backup(&proj, SESSIONS, false).unwrap().unwrap();
        assert!(w.service.project_state().projects.contains_key(&proj));

        let listed = w.service.list_projects().unwrap();
        assert_eq!(listed.len(), 1);
        w.service.delete_project_backup(&listed[0]).unwrap();
        assert!(w.mock.lock().unwrap().files.is_empty());
        assert!(w.service.list_projects().unwrap().is_empty());
        assert!(
            !w.service.project_state().projects.contains_key(&proj),
            "the state of the deleted last backup is dropped"
        );
        std::thread::sleep(Duration::from_millis(1100));
        let again = w.backup(&proj, SESSIONS, false).unwrap();
        assert!(again.is_some(), "an unchanged project is uploaded again");

        // Already gone: success, and the newer backup's state is left alone.
        w.service.delete_project_backup(&out.backup).unwrap();
        assert!(w.service.project_state().projects.contains_key(&proj));
        assert_eq!(w.service.list_projects().unwrap().len(), 1);
    }

    #[test]
    fn deleting_an_older_backup_keeps_the_recorded_state() {
        let w = world();
        let proj = w.tmp.path().join("proj");
        fs::create_dir_all(&proj).unwrap();
        session(&w.agent, "s1", &proj, "");
        w.backup(&proj, SESSIONS, false).unwrap().unwrap();
        let older = w.service.list_projects().unwrap().remove(0);
        // Backup names carry the time in seconds; the state is matched by name.
        std::thread::sleep(Duration::from_millis(1100));
        session(&w.agent, "s2", &proj, "");
        w.backup(&proj, SESSIONS, false).unwrap().unwrap();

        w.service.delete_project_backup(&older).unwrap();
        assert_eq!(w.service.list_projects().unwrap().len(), 1);
        assert!(
            w.backup(&proj, SESSIONS, false).unwrap().is_none(),
            "the latest backup is still there, so nothing is due"
        );
    }

    #[test]
    fn only_project_backups_can_be_deleted() {
        let w = world();
        let mut f = file("cfg", "2030-01-01T00:00:00.000Z");
        f.props = BTreeMap::from([
            ("kind".into(), "config".into()),
            ("host".into(), "h".into()),
            ("pid".into(), "p".into()),
        ]);
        w.mock
            .lock()
            .unwrap()
            .files
            .insert(f.id.clone(), (f.clone(), vec![1]));
        let backup = ProjectBackup::from_file(f).unwrap();
        let err = w.service.delete_project_backup(&backup).unwrap_err();
        assert!(matches!(err, Error::InvalidBackup(_)), "{err}");
        assert_eq!(w.mock.lock().unwrap().files.len(), 1);
    }

    // ------------------------------------------------------------ rejected archives

    fn manifest_for(files: &[(&str, &[u8])]) -> serde_json::Value {
        let listed: Vec<_> = files
            .iter()
            .map(|(p, d)| {
                serde_json::json!({
                    "path": p, "size": d.len(), "sha256": hex(&Sha256::digest(d)), "mode": 0o644
                })
            })
            .collect();
        serde_json::json!({
            "format": 1, "kind": "project", "created": "2026-01-01T00:00:00Z", "host": "h",
            "app_version": "0", "omp_version": "0", "project": "/p", "name": "p",
            "fingerprint": "f", "omp": { "files": listed }, "sessions": null
        })
    }

    fn put_backup(w: &World, archive: Vec<u8>) -> ProjectBackup {
        let mut f = file("crafted", "2030-01-01T00:00:00.000Z");
        f.name = format!("{PROJECT_PREFIX}crafted.tar.gz");
        f.size = archive.len() as u64;
        f.props = BTreeMap::from([
            ("kind".into(), "project".into()),
            ("host".into(), "h".into()),
            ("pid".into(), "p".into()),
        ]);
        w.mock
            .lock()
            .unwrap()
            .files
            .insert(f.id.clone(), (f.clone(), archive));
        ProjectBackup::from_file(f).unwrap()
    }

    #[test]
    fn malicious_or_tampered_project_archives_are_rejected() {
        let w = world();
        let target = w.tmp.path().join("target");
        let file = tar::EntryType::Regular;
        let manifest = |m: &serde_json::Value| m.to_string().into_bytes();
        let good_files: &[(&str, &[u8])] = &[("a.md", b"alpha")];
        let good = manifest_for(good_files);

        // Control: the helpers build an archive that is accepted.
        let ok = crafted(&[
            ("omp/a.md", file, b"alpha"),
            ("manifest.json", file, &manifest(&good)),
        ]);
        let backup = put_backup(&w, ok);
        assert!(w.prepare(&backup, &target, OMP, &w.root).is_ok());

        let mut wrong_kind = good.clone();
        wrong_kind["kind"] = "config".into();
        let mut future = good.clone();
        future["format"] = 2.into();
        let mut missing_member = good.clone();
        missing_member["omp"]["files"] =
            manifest_for(&[("a.md", b"alpha"), ("b.md", b"beta")])["omp"]["files"].clone();
        let g = manifest(&good);
        let cases: Vec<(&str, Vec<u8>)> = vec![
            (
                "path traversal",
                crafted(&[("omp/../../evil", file, b"x"), ("manifest.json", file, &g)]),
            ),
            (
                "absolute path",
                crafted(&[("/etc/evil", file, b"x"), ("manifest.json", file, &g)]),
            ),
            (
                "symlink member",
                crafted(&[
                    ("omp/a.md", file, b"alpha"),
                    ("omp/link", tar::EntryType::Symlink, b""),
                    ("manifest.json", file, &g),
                ]),
            ),
            (
                "hard link member",
                crafted(&[
                    ("omp/a.md", tar::EntryType::Link, b""),
                    ("manifest.json", file, &g),
                ]),
            ),
            (
                "member the manifest does not list",
                crafted(&[
                    ("omp/a.md", file, b"alpha"),
                    ("omp/extra.md", file, b"x"),
                    ("manifest.json", file, &g),
                ]),
            ),
            (
                "content differs from its sha256",
                crafted(&[("omp/a.md", file, b"alphA"), ("manifest.json", file, &g)]),
            ),
            ("no manifest", crafted(&[("omp/a.md", file, b"alpha")])),
            (
                "two manifests",
                crafted(&[
                    ("omp/a.md", file, b"alpha"),
                    ("manifest.json", file, &g),
                    ("manifest.json", file, &g),
                ]),
            ),
            (
                "wrong kind",
                crafted(&[
                    ("omp/a.md", file, b"alpha"),
                    ("manifest.json", file, &manifest(&wrong_kind)),
                ]),
            ),
            (
                "newer format",
                crafted(&[
                    ("omp/a.md", file, b"alpha"),
                    ("manifest.json", file, &manifest(&future)),
                ]),
            ),
            (
                "listed member missing",
                crafted(&[
                    ("omp/a.md", file, b"alpha"),
                    ("manifest.json", file, &manifest(&missing_member)),
                ]),
            ),
            (
                "entry outside the four prefixes",
                crafted(&[
                    ("agent/config.yml", file, b"x"),
                    ("omp/a.md", file, b"alpha"),
                    ("manifest.json", file, &g),
                ]),
            ),
            (
                "session id with a path in it",
                crafted(&[
                    ("sessions/../session.jsonl", file, b"x"),
                    ("omp/a.md", file, b"alpha"),
                    ("manifest.json", file, &g),
                ]),
            ),
            (
                "blob not named by its hash",
                crafted(&[("blobs/notahash", file, b"x"), ("manifest.json", file, &g)]),
            ),
        ];
        let fresh = w.tmp.path().join("fresh-target");
        for (what, bytes) in cases {
            let backup = put_backup(&w, bytes);
            let err = w
                .prepare(&backup, &fresh, OMP, &w.root)
                .err()
                .unwrap_or_else(|| panic!("{what}: accepted"));
            assert!(matches!(err, Error::InvalidBackup(_)), "{what}: {err}");
            assert!(
                !fresh.exists() && !w.tmp.path().join("evil").exists(),
                "{what}"
            );
            assert_eq!(w.staging(), 0, "{what}: staging cleaned");
        }
        let err = w
            .prepare(
                &put_backup(
                    &w,
                    crafted(&[
                        ("omp/a.md", file, b"alpha"),
                        (
                            "manifest.json",
                            file,
                            &manifest(&{
                                let mut m = good.clone();
                                m["omp"]["files"][0]["sha256"] = "0".repeat(64).into();
                                m
                            }),
                        ),
                    ]),
                ),
                &fresh,
                OMP,
                &w.root,
            )
            .err()
            .unwrap();
        assert!(err.to_string().contains("checksum"), "{err}");
    }

    #[test]
    fn cancelled_backup_uploads_nothing_and_leaves_no_staging_or_state() {
        let w = world();
        let proj = w.tmp.path().join("proj");
        write(&proj.join(".omp/AGENTS.md"), "x\n");
        let err = w
            .service
            .backup_project(
                &w.root,
                &proj,
                OMP,
                &BTreeSet::new(),
                true,
                &AtomicBool::new(true),
                &quiet,
            )
            .unwrap_err();
        assert!(matches!(err, Error::Cancelled), "{err}");
        assert!(w.mock.lock().unwrap().files.is_empty());
        assert_eq!(w.staging(), 0);
        assert!(w.service.project_state().projects.is_empty());
    }

    /// A directory handle that can have its modification time set (Windows needs backup
    /// semantics and write access to open a directory at all).
    fn open_dir_for_times(dir: &Path) -> fs::File {
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
            fs::OpenOptions::new()
                .write(true)
                .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
                .open(dir)
                .unwrap()
        }
        #[cfg(not(windows))]
        {
            fs::File::open(dir).unwrap()
        }
    }

    #[test]
    fn stale_staging_directories_are_swept_at_job_start() {
        let w = world();
        let cache = w.tmp.path().join("cache");
        let (stale, fresh) = (cache.join("stale"), cache.join("fresh"));
        write(&stale.join("leftover.tar.gz"), "partial");
        write(&fresh.join("running.tar.gz"), "in use");
        let two_days_ago = std::time::SystemTime::now() - Duration::from_secs(48 * 3600);
        open_dir_for_times(&stale)
            .set_modified(two_days_ago)
            .unwrap();
        let proj = w.tmp.path().join("proj");
        write(&proj.join(".omp/AGENTS.md"), "x\n");
        w.backup(&proj, OMP, false).unwrap().unwrap();
        assert!(!stale.exists(), "a day-old leftover is removed");
        assert!(fresh.exists(), "a recent staging directory is left alone");
    }

    // ------------------------------------------------------------ Drive transport

    /// Deterministic incompressible bytes.
    fn noise(len: usize) -> Vec<u8> {
        let mut x = 0x9E37_79B9_7F4A_7C15u64;
        (0..len)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                (x >> 24) as u8
            })
            .collect()
    }

    #[test]
    fn resumable_upload_resends_after_a_short_ack_and_resumes_after_interruptions() {
        // 20 MiB = chunks of 8 + 8 + 4 MiB. #1 is acknowledged 256 KiB short; #2 is stored but
        // its answer is lost; #3 (the last) hits a 503.
        let mock = Mock {
            faults: BTreeMap::from([
                (1, Fault::ShortAck),
                (2, Fault::DropAfterStore),
                (3, Fault::Http503),
            ]),
            ..Mock::default()
        };
        let w = world_with(mock);
        let data = noise(20 << 20);
        let path = w.tmp.path().join("big.bin");
        fs::write(&path, &data).unwrap();
        let account = w.service.store.load().unwrap().unwrap();
        let mut seen = Vec::new();
        let uploaded = w
            .service
            .drive(&account)
            .upload_file(
                "omp-project-big.tar.gz",
                &BTreeMap::from([("kind".into(), "project".into())]),
                &mut fs::File::open(&path).unwrap(),
                data.len() as u64,
                &AtomicBool::new(false),
                &mut |done| seen.push(done),
            )
            .unwrap();
        let total = data.len();
        let m = w.mock.lock().unwrap();
        assert!(m.files[&uploaded.id].1 == data, "uploaded bytes differ");
        assert_eq!(uploaded.size, total as u64);
        assert_eq!(m.files[&uploaded.id].0.props["kind"], "project");
        let acked = (8 << 20) - 256 * 1024;
        assert_eq!(m.range_log[0], format!("bytes 0-{}/{total}", (8 << 20) - 1));
        assert_eq!(
            m.range_log[1],
            format!("bytes {acked}-{}/{total}", acked + (8 << 20) - 1),
            "resends from the acknowledged byte"
        );
        assert_eq!(
            m.range_log[2],
            format!("bytes */{total}"),
            "asks Drive where it stands"
        );
        let stored = acked + (8 << 20);
        assert_eq!(
            m.range_log[3],
            format!("bytes {stored}-{}/{total}", total - 1),
            "continues after the stored-but-unanswered chunk"
        );
        assert_eq!(m.range_log[4], format!("bytes */{total}"));
        assert_eq!(
            m.range_log[5], m.range_log[3],
            "the 503 chunk is sent again"
        );
        assert_eq!(m.range_log.len(), 6);
        assert_eq!(*seen.last().unwrap(), total as u64);
    }

    #[test]
    fn download_resumes_with_a_range_after_a_dropped_connection() {
        let mut mock = Mock::default();
        let data = noise(3 << 20);
        let mut f = file("big", "2030-01-01T00:00:00.000Z");
        f.size = data.len() as u64;
        mock.files.insert(f.id.clone(), (f.clone(), data.clone()));
        mock.cut_downloads = [1_000_000].into();
        let w = world_with(mock);
        let account = w.service.store.load().unwrap().unwrap();
        let path = w.tmp.path().join("out.bin");
        let mut out = fs::File::create(&path).unwrap();
        w.service
            .drive(&account)
            .download_to(
                "big",
                data.len() as u64,
                &mut out,
                &AtomicBool::new(false),
                &mut |_| {},
            )
            .unwrap();
        assert!(fs::read(&path).unwrap() == data);
    }

    #[test]
    fn cancel_stops_an_upload_between_chunks() {
        let w = world();
        let data = noise(9 << 20);
        let path = w.tmp.path().join("big.bin");
        fs::write(&path, &data).unwrap();
        let account = w.service.store.load().unwrap().unwrap();
        let cancel = AtomicBool::new(false);
        let err = w
            .service
            .drive(&account)
            .upload_file(
                "omp-project-x.tar.gz",
                &BTreeMap::new(),
                &mut fs::File::open(&path).unwrap(),
                data.len() as u64,
                &cancel,
                &mut |_| cancel.store(true, std::sync::atomic::Ordering::Relaxed),
            )
            .unwrap_err();
        assert!(matches!(err, Error::Cancelled), "{err}");
        assert!(w.mock.lock().unwrap().files.is_empty());
    }

    // ------------------------------------------------------------ real omp

    /// A sandbox for the real `omp`: its own agent dir and home (with omp's default `.omp` config
    /// directory). `None` (with a message) when omp is not installed.
    fn real_omp(base: &Path, name: &str) -> Option<(Omp, PathBuf)> {
        let Some(omp) = Omp::detect() else {
            eprintln!("omp not found on PATH; skipping the session restore test");
            return None;
        };
        Some(sandboxed(omp, base, name))
    }

    /// `omp` (the real one, or a wrapper that runs it) with its own agent dir and home below
    /// `base/name`.
    fn sandboxed(omp: Omp, base: &Path, name: &str) -> (Omp, PathBuf) {
        let base = base.join(name);
        let (agent, home) = (base.join("agent"), base.join("home"));
        for d in [&agent, &home] {
            fs::create_dir_all(d).unwrap();
        }
        link_natives(&home);
        // `PI_CONFIG_DIR` is relative to the home directory (the default `.omp`, where the natives
        // live); Windows omp reads the home from `USERPROFILE`.
        let omp = omp
            .with_env("PI_CODING_AGENT_DIR", &agent)
            .with_env("PI_CONFIG_DIR", ".omp")
            .with_env("HOME", &home)
            .with_env("USERPROFILE", &home);
        (omp, agent)
    }

    /// omp unpacks a ~120 MB native addon into `$HOME/.omp/natives` on first use, which a
    /// sandboxed HOME would repeat for every test. Hard links to the installed copy make it
    /// instant (the installed files are only linked, never written). Best effort: when it
    /// fails omp extracts its own copy into the sandbox.
    fn link_natives(sandbox_home: &Path) {
        let Some(real) = dirs::home_dir().map(|h| h.join(".omp/natives")) else {
            return;
        };
        let Ok(versions) = fs::read_dir(&real) else {
            return;
        };
        for version in versions.flatten().filter(|v| v.path().is_dir()) {
            let dest = sandbox_home.join(".omp/natives").join(version.file_name());
            if fs::create_dir_all(&dest).is_err() {
                continue;
            }
            for f in fs::read_dir(version.path()).into_iter().flatten().flatten() {
                let _ = fs::hard_link(f.path(), dest.join(f.file_name()));
            }
        }
    }

    #[test]
    fn sessions_restore_through_omp_keeping_or_replacing_the_id() {
        let w = world();
        // /tmp is a small tmpfs and omp's sandbox HOME is big: keep it on the disk. CI runners
        // may not have ~/.cache yet.
        let cache = dirs::cache_dir().unwrap();
        fs::create_dir_all(&cache).unwrap();
        let disk = tempfile::Builder::new()
            .prefix("omp-deck-sync-test-")
            .tempdir_in(&cache)
            .unwrap();
        let Some((omp, _)) = real_omp(disk.path(), "same") else {
            return;
        };
        let src = w.tmp.path().join("src");
        fs::create_dir_all(&src).unwrap();
        let blob_name = blob(&w.agent, "a picture");
        let original = session(&w.agent, "01920000-0000-7000-8000-000000000001", &src, "");
        let stem = original.with_extension("");
        write(
            &stem.join("sub.jsonl"),
            &format!("{{\"img\":\"blob:sha256:{blob_name}\"}}\n"),
        );
        write(&stem.join("files/out.txt"), "artifact text");
        touch(&original, BACKED_UP_AT);
        let before = fs::read(&original).unwrap();
        w.backup(&src, SESSIONS, false).unwrap().unwrap();
        let backup = w.service.list_projects().unwrap().remove(0);
        assert_eq!(backup.sessions, Some(1));

        // 1. The id exists in the source project's dir: the copy must get a fresh id and the
        //    original must stay exactly as it is.
        let dst = w.tmp.path().join("dst");
        fs::create_dir_all(&dst).unwrap();
        let omp = omp.with_env("PI_CODING_AGENT_DIR", &w.agent);
        let prepared = w.prepare(&backup, &dst, SESSIONS, &w.root).unwrap();
        assert_eq!(prepared.plan.sessions.len(), 1);
        assert_eq!(prepared.plan.sessions[0].local.len(), 1);
        assert_eq!(prepared.plan.sessions[0].modified_unix, BACKED_UP_AT);
        let report = w
            .service
            .apply_project_restore(
                prepared,
                &BTreeMap::from([(
                    "01920000-0000-7000-8000-000000000001".to_owned(),
                    ConflictChoice::KeepBoth,
                )]),
                Some(&omp),
                &AtomicBool::new(false),
                &quiet,
            )
            .unwrap();
        assert!(
            report.sessions_failed.is_empty(),
            "{:?}",
            report.sessions_failed
        );
        assert_eq!(report.sessions_added.len(), 1);
        let added = &report.sessions_added[0];
        assert!(added.is_file() && *added != original);
        let header = header_of(added);
        assert_eq!(header["cwd"], dst.display().to_string());
        let new_id = header["id"].as_str().unwrap();
        assert_ne!(new_id, "01920000-0000-7000-8000-000000000001");
        assert_eq!(new_id.len(), 36);
        assert_eq!(new_id.as_bytes()[14], b'7', "UUIDv7-shaped");
        assert_eq!(
            mtime(added),
            BACKED_UP_AT,
            "the copy keeps the backed-up age"
        );
        assert!(
            fs::read(&original).unwrap() == before,
            "the original is untouched"
        );
        assert!(stem.join("sub.jsonl").is_file());
        let new_stem = added.with_extension("");
        assert_eq!(read(&new_stem.join("files/out.txt")), "artifact text");
        assert!(new_stem.join("sub.jsonl").is_file());
        assert!(w.agent.join("blobs").join(&blob_name).is_file());
        let groups = sessions::scan(&w.root);
        assert!(groups.iter().any(|g| g.cwd == dst && g.sessions.len() == 1));
        assert!(groups.iter().any(|g| g.cwd == src && g.sessions.len() == 1));

        // 2. A machine that has never seen the id keeps it; the blob arrives in its store.
        let (omp2, agent2) = real_omp(disk.path(), "other").unwrap();
        let root2 = SessionsRoot::at(agent2.join("sessions"));
        let dst2 = w.tmp.path().join("dst2");
        let prepared = w.prepare(&backup, &dst2, SESSIONS, &root2).unwrap();
        assert!(prepared.plan.create_target);
        assert!(prepared.plan.sessions[0].local.is_empty());
        let report = w
            .service
            .apply_project_restore(
                prepared,
                &BTreeMap::new(),
                Some(&omp2),
                &AtomicBool::new(false),
                &quiet,
            )
            .unwrap();
        assert!(
            report.sessions_failed.is_empty(),
            "{:?}",
            report.sessions_failed
        );
        let added = &report.sessions_added[0];
        assert!(added.starts_with(agent2.join("sessions")));
        let header = header_of(added);
        assert_eq!(header["id"], "01920000-0000-7000-8000-000000000001");
        assert_eq!(header["cwd"], dst2.display().to_string());
        assert!(agent2.join("blobs").join(&blob_name).is_file());
        assert_eq!(
            read(&added.with_extension("").join("files/out.txt")),
            "artifact text"
        );

        // Sessions need omp.
        let prepared = w.prepare(&backup, &dst2, SESSIONS, &root2).unwrap();
        assert!(
            w.service
                .apply_project_restore(
                    prepared,
                    &BTreeMap::new(),
                    None,
                    &AtomicBool::new(false),
                    &quiet
                )
                .is_err()
        );
    }

    /// A session `id` backed up from `src` (with its jsonl mtime set to `BACKED_UP_AT`), then
    /// edited locally (extra line, mtime `LOCAL_AT`). Returns (backup, local path, bytes of the
    /// edited file).
    fn edited_after_backup(w: &World, src: &Path, id: &str) -> (ProjectBackup, PathBuf, Vec<u8>) {
        let local = session(&w.agent, id, src, "");
        write(&local.with_extension("").join("files/out.txt"), "artifact");
        touch(&local, BACKED_UP_AT);
        w.backup(src, SESSIONS, false).unwrap().unwrap();
        let backup = w.service.list_projects().unwrap().remove(0);
        let mut f = fs::OpenOptions::new().append(true).open(&local).unwrap();
        f.write_all(b"{\"type\":\"local edit\"}\n").unwrap();
        drop(f);
        touch(&local, LOCAL_AT);
        let edited = fs::read(&local).unwrap();
        (backup, local, edited)
    }

    const BACKED_UP_AT: i64 = 1_700_000_000;
    const LOCAL_AT: i64 = 1_800_000_000;
    const ID: &str = "01920000-0000-7000-8000-0000000000aa";

    fn touch(path: &Path, unix: i64) {
        fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(
                std::time::SystemTime::UNIX_EPOCH
                    + Duration::from_secs(u64::try_from(unix).unwrap()),
            )
            .unwrap();
    }

    fn mtime(path: &Path) -> i64 {
        i64::try_from(
            fs::metadata(path)
                .unwrap()
                .modified()
                .unwrap()
                .duration_since(std::time::SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_secs(),
        )
        .unwrap()
    }

    #[test]
    fn a_local_session_with_the_same_id_is_kept_unless_told_otherwise() {
        let w = world();
        let src = w.tmp.path().join("src");
        fs::create_dir_all(&src).unwrap();
        let (backup, local, edited) = edited_after_backup(&w, &src, ID);
        let marker = w.tmp.path().join("omp-ran");
        // Must never run: nothing is to be written for a kept session.
        #[cfg(unix)]
        let omp = fake_omp(
            w.tmp.path(),
            "never.sh",
            &format!("touch {}\nexit 1", marker.display()),
        );
        #[cfg(not(unix))]
        let omp = Omp::at(&marker);
        let target = w.tmp.path().join("dst");
        let prepared = w.prepare(&backup, &target, SESSIONS, &w.root).unwrap();
        let copies = &prepared.plan.sessions[0].local;
        assert_eq!(copies.len(), 1);
        assert_eq!(copies[0].path, local);
        assert_eq!(copies[0].cwd.as_deref(), Some(src.as_path()));
        assert_eq!(copies[0].modified_unix, LOCAL_AT);
        assert_eq!(
            copies[0].bytes,
            edited.len() as u64 + "artifact".len() as u64
        );
        assert_eq!(prepared.plan.sessions[0].modified_unix, BACKED_UP_AT);
        // No entry in the map is the same as `KeepLocal`.
        let report = w
            .service
            .apply_project_restore(
                prepared,
                &BTreeMap::new(),
                Some(&omp),
                &AtomicBool::new(false),
                &quiet,
            )
            .unwrap();
        assert_eq!(report.sessions_kept, ["t"]);
        assert!(report.sessions_added.is_empty() && report.sessions_failed.is_empty());
        assert_eq!(report.sessions_replaced, 0);
        assert!(!marker.exists(), "omp was started");
        assert!(fs::read(&local).unwrap() == edited);
        assert_eq!(mtime(&local), LOCAL_AT);
    }

    #[test]
    fn overwriting_saves_the_local_copy_then_restores_the_backup_with_its_id_and_age() {
        let w = world();
        let cache = dirs::cache_dir().unwrap();
        fs::create_dir_all(&cache).unwrap();
        let disk = tempfile::Builder::new()
            .prefix("omp-deck-sync-test-")
            .tempdir_in(&cache)
            .unwrap();
        let Some((omp, _)) = real_omp(disk.path(), "overwrite") else {
            return;
        };
        let src = w.tmp.path().join("src");
        fs::create_dir_all(&src).unwrap();
        let (backup, local, edited) = edited_after_backup(&w, &src, ID);
        let prepared = w.prepare(&backup, &src, SESSIONS, &w.root).unwrap();
        let report = w
            .service
            .apply_project_restore(
                prepared,
                &BTreeMap::from([(ID.to_owned(), ConflictChoice::Overwrite)]),
                Some(&omp),
                &AtomicBool::new(false),
                &quiet,
            )
            .unwrap();
        assert!(
            report.sessions_failed.is_empty(),
            "{:?}",
            report.sessions_failed
        );
        assert_eq!(
            (report.sessions_added.len(), report.sessions_replaced),
            (1, 1)
        );
        assert!(report.sessions_kept.is_empty());
        // The backup is back under the same id (and name), with the backed-up age.
        let added = &report.sessions_added[0];
        assert_eq!(*added, local);
        let text = read(added);
        assert!(text.contains("\"content\":\"hi\"") && !text.contains("local edit"));
        assert_eq!(header_of(added)["id"], ID);
        assert_eq!(mtime(added), BACKED_UP_AT);
        assert_eq!(
            read(&added.with_extension("").join("files/out.txt")),
            "artifact"
        );
        // The edited copy was saved first, in the restore's safety directory.
        let safety = report.safety_dir.as_ref().unwrap();
        let tar = safety.join("sessions").join(format!("{ID}.tar"));
        let mut archive = tar::Archive::new(fs::File::open(&tar).unwrap());
        let saved: Vec<u8> = archive
            .entries()
            .unwrap()
            .map(|e| e.unwrap())
            .find(|e| e.path().unwrap().ends_with("session.jsonl"))
            .map(|mut e| {
                let mut data = Vec::new();
                e.read_to_end(&mut data).unwrap();
                data
            })
            .unwrap();
        assert!(saved == edited);
        let all: usize = sessions::scan(&w.root)
            .iter()
            .map(|g| g.sessions.len())
            .sum();
        assert_eq!(all, 1);
    }

    #[cfg(unix)]
    #[test]
    fn a_failed_overwrite_puts_the_local_copy_back_with_its_time() {
        let w = world();
        let cache = dirs::cache_dir().unwrap();
        fs::create_dir_all(&cache).unwrap();
        let disk = tempfile::Builder::new()
            .prefix("omp-deck-sync-test-")
            .tempdir_in(&cache)
            .unwrap();
        if Omp::detect().is_none() {
            eprintln!("omp not found on PATH; skipping the rollback test");
            return;
        }
        let src = w.tmp.path().join("src");
        fs::create_dir_all(&src).unwrap();
        let (backup, local, _) = edited_after_backup(&w, &src, ID);
        // An omp that is the real one except that unpacking the backup (its tar is built in
        // the staging `work` folder) fails; the safety tars unpack normally.
        let flaky = fake_omp(
            w.tmp.path(),
            "flaky.sh",
            "case \"$*\" in\n  \"session unpack \"*/work/*) echo 'error: disk is on fire' >&2; exit 1;;\nesac\n\
             exec omp \"$@\"",
        );
        let (omp, _) = sandboxed(flaky, disk.path(), "flaky");
        // The restore target is another folder: the copy must go back into its own cwd.
        let target = w.tmp.path().join("dst");
        let prepared = w.prepare(&backup, &target, SESSIONS, &w.root).unwrap();
        let report = w
            .service
            .apply_project_restore(
                prepared,
                &BTreeMap::from([(ID.to_owned(), ConflictChoice::Overwrite)]),
                Some(&omp),
                &AtomicBool::new(false),
                &quiet,
            )
            .unwrap();
        assert!(report.sessions_added.is_empty() && report.sessions_replaced == 0);
        assert_eq!(
            report.sessions_failed,
            [(
                "t".to_owned(),
                "disk is on fire; the local copy was put back".to_owned()
            )]
        );
        assert!(local.is_file(), "the local copy is back");
        // omp rewrites the header while unpacking: the content is what counts.
        assert!(read(&local).contains("local edit"));
        assert_eq!(mtime(&local), LOCAL_AT);
        assert_eq!(header_of(&local)["cwd"], src.display().to_string());
        assert_eq!(
            read(&local.with_extension("").join("files/out.txt")),
            "artifact"
        );
        let safety = report.safety_dir.unwrap();
        assert!(safety.join("sessions").join(format!("{ID}.tar")).is_file());
        let all: usize = sessions::scan(&w.root)
            .iter()
            .map(|g| g.sessions.len())
            .sum();
        assert_eq!(all, 1);
    }

    // ------------------------------------------------------------ links, skips, conflicts

    #[test]
    #[cfg(unix)]
    fn symlinked_omp_and_artifact_dirs_are_never_followed() {
        let w = world();
        let outside = w.tmp.path().join("outside");
        write(&outside.join("secret.md"), "secret");
        let proj = w.tmp.path().join("proj");
        fs::create_dir_all(&proj).unwrap();
        symlink(&outside, proj.join(".omp")).unwrap();
        assert_eq!(measure(&w.root, &proj).unwrap().omp, OmpDir::Symlink);
        assert!(matches!(
            w.backup(&proj, OMP, true),
            Err(Error::NothingToBackUp(_))
        ));

        let main = session(&w.agent, "s1", &proj, "");
        write(&outside.join("art.txt"), "art");
        symlink(&outside, main.with_extension("")).unwrap();
        let out = w.backup(&proj, BOTH, false).unwrap().unwrap();
        let names: Vec<String> = members(&only_archive(&w))
            .into_iter()
            .map(|m| m.0)
            .collect();
        assert_eq!(names, ["sessions/s1/session.jsonl", "manifest.json"]);
        assert!(
            out.skipped
                .iter()
                .any(|s| s.starts_with(".omp: symbolic link")),
            "{:?}",
            out.skipped
        );
        assert!(
            out.skipped
                .iter()
                .any(|s| s.contains("artifacts folder is a symbolic link")),
            "{:?}",
            out.skipped
        );
    }

    #[test]
    #[cfg(unix)]
    fn symlinked_artifact_folders_are_listed_as_skipped() {
        let w = world();
        let proj = w.tmp.path().join("proj");
        fs::create_dir_all(&proj).unwrap();
        let main = session(&w.agent, "s1", &proj, "");
        let outside = w.tmp.path().join("outside");
        write(&outside.join("art.txt"), "art");
        symlink(&outside, main.with_extension("")).unwrap();
        let out = w.backup(&proj, SESSIONS, false).unwrap().unwrap();
        assert_eq!(
            out.skipped,
            ["session “t”: artifacts folder is a symbolic link, not backed up"]
        );
    }

    #[test]
    fn unresolved_blob_state_is_the_one_seen_while_packing() {
        let w = world();
        let proj = w.tmp.path().join("proj");
        fs::create_dir_all(&proj).unwrap();
        let name = hex(&Sha256::digest(b"what the name promises"));
        // The blob exists but its content is not what its name says: damaged.
        write(&w.agent.join("blobs").join(&name), "damaged");
        session(
            &w.agent,
            "s1",
            &proj,
            &format!("{{\"ref\":\"blob:sha256:{name}\"}}\n"),
        );
        let meta = fs::metadata(w.agent.join("blobs").join(&name)).unwrap();
        let mtime = meta
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let out = w.backup(&proj, SESSIONS, false).unwrap().unwrap();
        assert_eq!(
            out.synced.unresolved_blobs,
            BTreeMap::from([(name.clone(), format!("{}:{mtime}", meta.len()))])
        );
        assert!(w.backup(&proj, SESSIONS, false).unwrap().is_none());
        // Changed since: noticed, because the recorded state is not a later sample.
        write(&w.agent.join("blobs").join(&name), "damaged differently");
        assert!(w.backup(&proj, SESSIONS, false).unwrap().is_some());
    }

    #[test]
    fn an_upload_that_is_never_acknowledged_ends_instead_of_hanging() {
        let w = world_with(Mock {
            always_308: true,
            ..Mock::default()
        });
        let err = small_upload(&w, &AtomicBool::new(false)).unwrap_err();
        assert!(
            matches!(&err, Error::Network(m) if m.contains("acknowledged no data")),
            "{err}"
        );
        assert!(w.mock.lock().unwrap().range_log.len() < 20);
    }

    #[test]
    fn a_server_that_ignores_range_and_cuts_at_the_same_byte_ends_the_download() {
        let data = noise(3 << 20);
        let mut f = file("big", "2030-01-01T00:00:00.000Z");
        f.size = data.len() as u64;
        let mut mock = Mock {
            ignore_range: true,
            cut_downloads: [1_000_000; 50].into(),
            ..Mock::default()
        };
        mock.files.insert(f.id.clone(), (f, data.clone()));
        let w = world_with(mock);
        let account = w.service.store.load().unwrap().unwrap();
        let mut out = fs::File::create(w.tmp.path().join("out.bin")).unwrap();
        let err = w
            .service
            .drive(&account)
            .download_to(
                "big",
                data.len() as u64,
                &mut out,
                &AtomicBool::new(false),
                &mut |_| {},
            )
            .unwrap_err();
        assert!(matches!(err, Error::Network(_)), "{err}");
        let left = w.mock.lock().unwrap().cut_downloads.len();
        assert!(left > 40, "gave up after {} attempts", 50 - left);
    }

    #[test]
    #[cfg(unix)]
    fn left_out_omp_entries_are_reported_with_reasons() {
        let w = world();
        let proj = w.tmp.path().join("proj");
        let omp = proj.join(".omp");
        write(&omp.join("ok.md"), "fine");
        write(&omp.join("back\\slash.md"), "x");
        let bad = std::ffi::OsStr::from_bytes(b"bad\xff.md");
        fs::write(omp.join(bad), "x").unwrap();
        symlink(omp.join("ok.md"), omp.join("link.md")).unwrap();
        let made = std::process::Command::new("mkfifo")
            .arg(omp.join("fifo"))
            .status()
            .unwrap();
        assert!(made.success());

        let out = w.backup(&proj, OMP, false).unwrap().unwrap();
        let mut skipped = out.skipped.clone();
        skipped.sort();
        assert_eq!(
            skipped,
            [
                "back\\slash.md: backslash in the file name",
                "bad\u{fffd}.md: file name is not valid UTF-8",
                "fifo: not a regular file",
                "link.md: symbolic link",
            ]
        );
        let names: Vec<String> = members(&only_archive(&w))
            .into_iter()
            .map(|m| m.0)
            .collect();
        assert_eq!(names, ["omp/ok.md", "manifest.json"]);
    }

    #[test]
    #[cfg(unix)]
    fn restore_never_writes_through_links_and_reports_clashes_in_the_plan() {
        let w = world();
        let proj = w.tmp.path().join("proj");
        for f in [
            "AGENTS.md",
            "hooks/pre.sh",
            "rules/r.md",
            "dir/f.md",
            "file.md",
        ] {
            write(&proj.join(".omp").join(f), "from backup");
        }
        w.backup(&proj, OMP, false).unwrap().unwrap();
        let backup = w.service.list_projects().unwrap().remove(0);

        let outside = w.tmp.path().join("outside");
        write(&outside.join("target.md"), "outside");
        let target = w.tmp.path().join("target");
        let local = target.join(".omp");
        fs::create_dir_all(&local).unwrap();
        symlink(&outside, local.join("hooks")).unwrap(); // a link on the way
        symlink(outside.join("target.md"), local.join("AGENTS.md")).unwrap(); // the file is a link
        write(&local.join("rules"), "a file where the backup has a folder");
        fs::create_dir_all(local.join("file.md")).unwrap(); // a folder where the backup has a file

        let prepared = w.prepare(&backup, &target, OMP, &w.root).unwrap();
        let plan = &prepared.plan;
        assert_eq!(plan.omp_added, ["dir/f.md"]);
        assert!(plan.omp_overwritten.is_empty());
        assert_eq!(
            plan.omp_skipped,
            [
                "AGENTS.md: a symbolic link is in the way",
                "file.md: a folder is in the way",
                "hooks/pre.sh: hooks is a symbolic link",
                "rules/r.md: rules is a file, the backup has a folder there",
            ]
        );
        let skipped = plan.omp_skipped.clone();
        let report = w
            .service
            .apply_project_restore(
                prepared,
                &BTreeMap::new(),
                None,
                &AtomicBool::new(false),
                &quiet,
            )
            .unwrap();
        assert_eq!((report.omp_written, report.omp_overwritten), (1, 0));
        assert_eq!(report.omp_skipped, skipped);
        assert!(report.safety_dir.is_none());
        assert_eq!(read(&local.join("dir/f.md")), "from backup");
        assert_eq!(
            read(&outside.join("target.md")),
            "outside",
            "not written through"
        );
        assert!(!outside.join("pre.sh").exists());
        assert!(
            fs::symlink_metadata(local.join("AGENTS.md"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(local.join("file.md").is_dir());

        // A `.omp` that is a link or a file, and a target that is a file, are refused up front.
        let linked = w.tmp.path().join("linked");
        fs::create_dir_all(&linked).unwrap();
        symlink(&outside, linked.join(".omp")).unwrap();
        let err = w.prepare(&backup, &linked, OMP, &w.root).err().unwrap();
        assert!(
            matches!(&err, Error::Io(m) if m.contains("symbolic link")),
            "{err}"
        );
        let filed = w.tmp.path().join("filed");
        write(&filed.join(".omp"), "a file");
        let err = w.prepare(&backup, &filed, OMP, &w.root).err().unwrap();
        assert!(
            matches!(&err, Error::Io(m) if m.contains("is a file")),
            "{err}"
        );
        let err = w
            .prepare(&backup, &filed.join(".omp"), OMP, &w.root)
            .err()
            .unwrap();
        assert!(
            matches!(&err, Error::Io(m) if m.contains("is a file")),
            "{err}"
        );
        assert_eq!(w.staging(), 0);
        assert_eq!(read(&outside.join("target.md")), "outside");
    }

    #[test]
    fn a_blob_that_appears_after_the_backup_makes_the_project_changed() {
        let w = world();
        let proj = w.tmp.path().join("proj");
        fs::create_dir_all(&proj).unwrap();
        let content = "an image that is still being written";
        let name = hex(&Sha256::digest(content.as_bytes()));
        session(
            &w.agent,
            "s1",
            &proj,
            &format!("{{\"ref\":\"blob:sha256:{name}\"}}\n"),
        );
        let first = w.backup(&proj, SESSIONS, false).unwrap().unwrap();
        assert_eq!(
            first.synced.unresolved_blobs,
            BTreeMap::from([(name.clone(), "missing".to_owned())])
        );
        assert!(w.backup(&proj, SESSIONS, false).unwrap().is_none());

        blob(&w.agent, content);
        let second = w.backup(&proj, SESSIONS, false).unwrap().unwrap();
        assert!(second.synced.unresolved_blobs.is_empty());
        let names: Vec<String> = {
            let m = w.mock.lock().unwrap();
            members(&m.files["new2"].1)
                .into_iter()
                .map(|m| m.0)
                .collect()
        };
        assert!(names.contains(&format!("blobs/{name}")), "{names:?}");
        assert!(w.backup(&proj, SESSIONS, false).unwrap().is_none());
    }

    #[test]
    fn sessions_with_members_over_2_gib_are_left_out_and_reported() {
        let w = world();
        let proj = w.tmp.path().join("proj");
        fs::create_dir_all(&proj).unwrap();
        let big = session(&w.agent, "big", &proj, "");
        fs::create_dir_all(big.with_extension("")).unwrap();
        fs::File::create(big.with_extension("").join("huge.bin"))
            .unwrap()
            .set_len((2 << 30) + 1)
            .unwrap();
        session(&w.agent, "small", &proj, "");
        let out = w.backup(&proj, SESSIONS, false).unwrap().unwrap();
        assert_eq!(out.backup.sessions, Some(1));
        assert_eq!(out.skipped.len(), 1, "{:?}", out.skipped);
        assert!(out.skipped[0].contains("huge.bin") && out.skipped[0].contains("2 GiB"));
        let names: Vec<String> = members(&only_archive(&w))
            .into_iter()
            .map(|m| m.0)
            .collect();
        assert_eq!(names, ["sessions/small/session.jsonl", "manifest.json"]);
    }

    #[test]
    fn an_archived_session_must_carry_the_id_it_is_listed_under() {
        let w = world();
        let target = w.tmp.path().join("target");
        let archive = |header_id: &str| {
            let data = format!("{{\"type\":\"session\",\"id\":\"{header_id}\",\"cwd\":\"/p\"}}\n");
            let mut m = manifest_for(&[]);
            m["omp"] = serde_json::Value::Null;
            m["sessions"] = serde_json::json!({
                "items": [{
                    "id": "a", "file_name": "t_a.jsonl", "title": "t", "modified_unix": 0,
                    "size": data.len(), "sha256": hex(&Sha256::digest(data.as_bytes())),
                    "artifacts": [], "blobs": []
                }],
                "blobs": []
            });
            let kind = tar::EntryType::Regular;
            crafted(&[
                ("sessions/a/session.jsonl", kind, data.as_bytes()),
                ("manifest.json", kind, m.to_string().as_bytes()),
            ])
        };
        let ok = put_backup(&w, archive("a"));
        let prepared = w.prepare(&ok, &target, SESSIONS, &w.root).unwrap();
        assert_eq!(prepared.plan.sessions[0].id, "a");
        assert!(prepared.plan.sessions[0].problem.is_none());
        drop(prepared);

        let forged = put_backup(&w, archive("b"));
        let err = w
            .prepare(&forged, &target, SESSIONS, &w.root)
            .err()
            .unwrap();
        assert!(
            matches!(&err, Error::InvalidBackup(m) if m.contains("says it is b")),
            "{err}"
        );
        assert_eq!(w.staging(), 0);
    }

    // ------------------------------------------------------------ transport retries

    fn small_upload(w: &World, cancel: &AtomicBool) -> Result<BackupFile> {
        let account = w.service.store.load().unwrap().unwrap();
        let path = w.tmp.path().join("small.bin");
        fs::write(&path, b"0123456789").unwrap();
        w.service.drive(&account).upload_file(
            "omp-project-small.tar.gz",
            &BTreeMap::new(),
            &mut fs::File::open(&path).unwrap(),
            10,
            cancel,
            &mut |_| {},
        )
    }

    #[test]
    fn download_retries_server_errors_and_progress_resets_the_failure_count() {
        let data = noise(3 << 20);
        let mut f = file("big", "2030-01-01T00:00:00.000Z");
        f.size = data.len() as u64;
        let mut mock = Mock::default();
        mock.files.insert(f.id.clone(), (f, data.clone()));
        let w = world_with(mock);
        let account = w.service.store.load().unwrap().unwrap();
        let fetch = || -> Result<Vec<u8>> {
            let path = w.tmp.path().join("out.bin");
            let mut out = fs::File::create(&path).unwrap();
            w.service.drive(&account).download_to(
                "big",
                data.len() as u64,
                &mut out,
                &AtomicBool::new(false),
                &mut |_| {},
            )?;
            Ok(fs::read(&path).unwrap())
        };

        // 429, 503 and a dropped connection before any byte: retried, then it completes.
        w.mock.lock().unwrap().get_faults = [429, 503, 0].into();
        assert!(fetch().unwrap() == data);

        // Seven interruptions in a row, each after some progress: more than the five failures
        // allowed in a row, but every request moved forward.
        w.mock.lock().unwrap().cut_downloads = [200_000; 7].into();
        assert!(fetch().unwrap() == data);

        // Five failures without progress end it.
        w.mock.lock().unwrap().get_faults = [503; 5].into();
        let err = fetch().unwrap_err();
        assert!(
            matches!(&err, Error::Network(m) if m.contains("503")),
            "{err}"
        );

        // Other statuses are not retried.
        w.mock.lock().unwrap().get_faults = [403].into();
        assert!(matches!(fetch().unwrap_err(), Error::Google(_)));
    }

    #[test]
    fn starting_an_upload_session_retries_and_can_be_cancelled() {
        let w = world_with(Mock {
            init_faults: [503, 0, 429].into(),
            ..Mock::default()
        });
        let uploaded = small_upload(&w, &AtomicBool::new(false)).unwrap();
        assert_eq!(w.mock.lock().unwrap().files[&uploaded.id].1, b"0123456789");

        w.mock.lock().unwrap().init_faults = [503; 5].into();
        let err = small_upload(&w, &AtomicBool::new(false)).unwrap_err();
        assert!(
            matches!(&err, Error::Network(m) if m.contains("503")),
            "{err}"
        );

        // Cancelled while backing off between attempts.
        w.mock.lock().unwrap().init_faults = [503; 5].into();
        let cancel = AtomicBool::new(false);
        let err = std::thread::scope(|s| {
            s.spawn(|| {
                std::thread::sleep(Duration::from_millis(25));
                cancel.store(true, std::sync::atomic::Ordering::Relaxed);
            });
            small_upload(&w, &cancel).unwrap_err()
        });
        assert!(matches!(err, Error::Cancelled), "{err}");
        assert_eq!(err.to_string(), "cancelled");
    }

    #[test]
    fn backup_fails_before_packing_when_google_cannot_be_reached() {
        let mut w = world();
        let dead = {
            let l = TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap()
        };
        w.service.endpoints.token = format!("http://{dead}/token");
        let proj = w.tmp.path().join("proj");
        write(&proj.join(".omp/AGENTS.md"), "x");
        let err = w.backup(&proj, OMP, true).unwrap_err();
        assert!(matches!(err, Error::Network(_)), "{err}");
        assert!(
            !w.tmp.path().join("cache").exists(),
            "nothing was staged or packed"
        );
        assert!(w.service.project_state().projects.is_empty());
    }

    #[test]
    fn progress_starts_at_zero_and_ends_complete() {
        let w = world();
        let proj = w.tmp.path().join("proj");
        write(&proj.join(".omp/AGENTS.md"), &"rules\n".repeat(1000));
        let events = Mutex::new(Vec::new());
        let record = |p: Progress| events.lock().unwrap().push(p);
        w.service
            .backup_project(
                &w.root,
                &proj,
                OMP,
                &BTreeSet::new(),
                true,
                &AtomicBool::new(false),
                &record,
            )
            .unwrap()
            .unwrap();
        let of = |stage| -> Vec<Progress> {
            events
                .lock()
                .unwrap()
                .iter()
                .copied()
                .filter(|p| p.stage == stage)
                .collect()
        };
        let up = of(Stage::Uploading);
        assert!(up[0].done == 0 && up[0].total > 0, "{up:?}");
        assert_eq!(up.last().unwrap().done, up[0].total);

        events.lock().unwrap().clear();
        let backup = w.service.list_projects().unwrap().remove(0);
        w.service
            .prepare_project_restore(
                &backup,
                &w.tmp.path().join("t"),
                OMP,
                &w.root,
                &AtomicBool::new(false),
                &record,
            )
            .unwrap();
        let verify = of(Stage::Verifying);
        assert_eq!(verify[0].done, 0);
        let last = verify.last().unwrap();
        assert!(last.total > 0 && last.done == last.total, "{verify:?}");
    }

    // ------------------------------------------------------------ restoring sessions

    #[cfg(unix)]
    fn fake_omp(dir: &Path, name: &str, body: &str) -> Omp {
        let path = dir.join(name);
        let mut stage = path.as_os_str().to_owned();
        stage.push(".src");
        fs::write(&stage, format!("#!/bin/sh\n{body}\n")).unwrap();
        // External copy: no write fd to the executed inode lives here (ETXTBSY with sibling forks).
        let status = std::process::Command::new("install")
            .args(["-m", "755"])
            .arg(&stage)
            .arg(&path)
            .status()
            .unwrap();
        assert!(status.success());
        Omp::at(path)
    }

    /// A project with `ids` sessions and `files` in `.omp`, backed up; its list entry.
    #[cfg(unix)]
    fn backed_up(w: &World, ids: &[&str], files: &[&str]) -> (PathBuf, ProjectBackup) {
        let proj = w.tmp.path().join("proj");
        fs::create_dir_all(&proj).unwrap();
        for id in ids {
            session(&w.agent, id, &proj, "");
        }
        for f in files {
            write(&proj.join(".omp").join(f), "x\n");
        }
        let parts = Parts {
            omp: !files.is_empty(),
            sessions: !ids.is_empty(),
            memory: false,
        };
        w.backup(&proj, parts, false).unwrap().unwrap();
        let backup = w.service.list_projects().unwrap().remove(0);
        (proj, backup)
    }

    #[cfg(unix)]
    const OK_OMP: &str = "echo /store/sessions/x/new.jsonl; echo 'Unpacked session (abc).' >&2";

    /// Every session of the plan restored next to a local copy of the same id.
    #[cfg(unix)]
    fn keep_both(prepared: &PreparedProjectRestore) -> BTreeMap<String, ConflictChoice> {
        prepared
            .plan
            .sessions
            .iter()
            .map(|s| (s.id.clone(), ConflictChoice::KeepBoth))
            .collect()
    }

    #[cfg(unix)]
    #[test]
    fn session_unpack_runs_in_the_prepared_store_with_its_scratch_in_the_staging_dir() {
        let w = world();
        let (_, backup) = backed_up(&w, &["s1"], &[]);
        let store = w.tmp.path().join("store");
        let root = SessionsRoot::at(store.join("sessions"));
        let seen = w.tmp.path().join("seen");
        let omp = fake_omp(
            w.tmp.path(),
            "omp.sh",
            &format!(
                "{{ echo \"PI=$PI_CODING_AGENT_DIR\"; echo \"TMPDIR=$TMPDIR\"; echo \"TMP=$TMP\"; \
                 echo \"TEMP=$TEMP\"; [ -d \"$TMPDIR\" ] && echo scratch-exists; echo \"ARGS=$*\"; }} > {}\n{OK_OMP}",
                seen.display()
            ),
        )
        // The caller's own store setting must not win over the one the restore was prepared for.
        .with_env("PI_CODING_AGENT_DIR", "/nonexistent");
        let target = w.tmp.path().join("target");
        let prepared = w.prepare(&backup, &target, SESSIONS, &root).unwrap();
        let report = w
            .service
            .apply_project_restore(
                prepared,
                &BTreeMap::new(),
                Some(&omp),
                &AtomicBool::new(false),
                &quiet,
            )
            .unwrap();
        assert!(
            report.sessions_failed.is_empty(),
            "{:?}",
            report.sessions_failed
        );
        assert_eq!(
            report.sessions_added,
            [PathBuf::from("/store/sessions/x/new.jsonl")]
        );
        let seen = read(&seen);
        let line = |k: &str| {
            seen.lines()
                .find_map(|l| l.strip_prefix(k))
                .unwrap_or_else(|| panic!("{k} missing in {seen}"))
                .to_owned()
        };
        assert_eq!(line("PI="), store.display().to_string());
        let scratch = line("TMPDIR=");
        assert!(
            scratch.starts_with(w.tmp.path().join("cache").to_str().unwrap())
                && scratch.ends_with("/tmp"),
            "{scratch}"
        );
        assert_eq!((line("TMP="), line("TEMP=")), (scratch.clone(), scratch));
        assert!(seen.contains("scratch-exists"));
        let args = line("ARGS=");
        assert!(
            args.starts_with("session unpack ")
                && args.ends_with(&format!("--cwd {}", target.display())),
            "{args}"
        );
        assert_eq!(w.staging(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn omp_failures_are_reported_as_one_clean_line() {
        let w = world();
        let (_, backup) = backed_up(&w, &["s1"], &[]);
        let omp = fake_omp(
            w.tmp.path(),
            "bad.sh",
            "echo '642 |     throw new Error(`Unsupported platform`)' >&2\n\
             echo '            ^' >&2\n\
             echo 'error: Failed to load the native addon for linux-x64.' >&2\n\
             echo '' >&2\necho 'Tried:' >&2\necho '- EDQUOT: unknown error, write' >&2\nexit 1",
        );
        let prepared = w
            .prepare(&backup, &w.tmp.path().join("t"), SESSIONS, &w.root)
            .unwrap();
        let keep_both = keep_both(&prepared);
        let report = w
            .service
            .apply_project_restore(
                prepared,
                &keep_both,
                Some(&omp),
                &AtomicBool::new(false),
                &quiet,
            )
            .unwrap();
        assert!(report.sessions_added.is_empty());
        assert_eq!(
            report.sessions_failed,
            [(
                "t".to_owned(),
                "Failed to load the native addon for linux-x64.".to_owned()
            )]
        );
    }

    #[cfg(unix)]
    #[test]
    fn left_out_sessions_are_neither_restored_nor_reported() {
        let w = world();
        let (_, backup) = backed_up(&w, &["s1", "s2", "s3"], &[]);
        let seen = w.tmp.path().join("seen");
        // Records the id of every pack omp is asked to unpack (`session unpack <tar> --cwd …`).
        let omp = fake_omp(
            w.tmp.path(),
            "omp.sh",
            &format!(
                "tar -xOf \"$3\" manifest.json >> {seen}\necho >> {seen}\n{OK_OMP}",
                seen = seen.display()
            ),
        );
        let root = SessionsRoot::at(w.tmp.path().join("store/sessions"));
        let target = w.tmp.path().join("target");
        let mut prepared = w.prepare(&backup, &target, SESSIONS, &root).unwrap();
        prepared.leave_out_sessions(&ids(&["s2", "not-in-the-backup"]));
        let planned: BTreeSet<&str> = prepared
            .plan
            .sessions
            .iter()
            .map(|s| s.id.as_str())
            .collect();
        assert_eq!(planned, BTreeSet::from(["s1", "s3"]));
        let report = w
            .service
            .apply_project_restore(
                prepared,
                &BTreeMap::new(),
                Some(&omp),
                &AtomicBool::new(false),
                &quiet,
            )
            .unwrap();
        assert!(
            report.sessions_failed.is_empty(),
            "{:?}",
            report.sessions_failed
        );
        assert_eq!(report.sessions_added.len(), 2);
        let unpacked: BTreeSet<String> = read(&seen)
            .lines()
            .filter(|l| !l.is_empty())
            .map(|l| {
                serde_json::from_str::<serde_json::Value>(l).unwrap()["id"]
                    .as_str()
                    .unwrap()
                    .to_owned()
            })
            .collect();
        assert_eq!(unpacked, ids(&["s1", "s3"]));

        // Leaving out every session needs no omp at all.
        let mut prepared = w.prepare(&backup, &target, SESSIONS, &root).unwrap();
        prepared.leave_out_sessions(&ids(&["s1", "s2", "s3"]));
        let report = w
            .service
            .apply_project_restore(
                prepared,
                &BTreeMap::new(),
                None,
                &AtomicBool::new(false),
                &quiet,
            )
            .unwrap();
        assert!(report.sessions_added.is_empty() && report.sessions_failed.is_empty());
    }

    /// A progress callback that cancels once `stage` has finished its first item.
    #[cfg(unix)]
    fn stop_after(stage: Stage, cancel: &AtomicBool) -> impl Fn(Progress) + Sync + '_ {
        move |p: Progress| {
            if p.stage == stage && p.done == 1 {
                cancel.store(true, std::sync::atomic::Ordering::Relaxed);
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn cancelling_keeps_what_was_already_restored() {
        let w = world();
        let (_, backup) = backed_up(&w, &["s1", "s2"], &["a.md", "b.md"]);
        let omp = fake_omp(w.tmp.path(), "ok.sh", OK_OMP);
        let both = Parts {
            omp: true,
            sessions: true,
            memory: false,
        };

        // Between the two sessions: the first is kept in the report.
        let cancel = AtomicBool::new(false);
        let target = w.tmp.path().join("t1");
        let prepared = w.prepare(&backup, &target, both, &w.root).unwrap();
        let keep_both = keep_both(&prepared);
        let report = w
            .service
            .apply_project_restore(
                prepared,
                &keep_both,
                Some(&omp),
                &cancel,
                &stop_after(Stage::RestoringSessions, &cancel),
            )
            .unwrap();
        assert!(report.cancelled);
        assert_eq!((report.omp_written, report.sessions_added.len()), (2, 1));
        assert!(report.sessions_failed.is_empty());

        // Between the two `.omp` files: one is written, sessions are not started.
        let cancel = AtomicBool::new(false);
        let target = w.tmp.path().join("t2");
        let prepared = w.prepare(&backup, &target, both, &w.root).unwrap();
        let report = w
            .service
            .apply_project_restore(
                prepared,
                &BTreeMap::new(),
                Some(&omp),
                &cancel,
                &stop_after(Stage::RestoringOmp, &cancel),
            )
            .unwrap();
        assert!(report.cancelled);
        assert_eq!(report.omp_written, 1);
        assert!(report.sessions_added.is_empty());
        assert_eq!(
            fs::read_dir(target.join(".omp")).unwrap().flatten().count(),
            1
        );

        // Before anything was written: a plain cancel, with the generic text.
        let target = w.tmp.path().join("t3");
        let prepared = w.prepare(&backup, &target, both, &w.root).unwrap();
        let err = w
            .service
            .apply_project_restore(
                prepared,
                &BTreeMap::new(),
                Some(&omp),
                &AtomicBool::new(true),
                &quiet,
            )
            .unwrap_err();
        assert_eq!(err.to_string(), "cancelled");
        assert!(!target.exists());
    }

    // ------------------------------------------------------------ sharpshooter memory

    const OMP_MEMORY: Parts = Parts {
        omp: true,
        sessions: false,
        memory: true,
    };

    /// The bank omp keeps for `project` in the store of `w`.
    fn bank(w: &World, project: &Path) -> PathBuf {
        sharpshooter::bank_dir(&w.root, project)
    }

    fn seed(dir: &Path, files: &[(&str, &str)]) {
        for (name, text) in files {
            write(&dir.join(name), text);
        }
    }

    fn newest(w: &World) -> ProjectBackup {
        w.service.list_projects().unwrap().remove(0)
    }

    fn apply(w: &World, prepared: PreparedProjectRestore) -> ProjectRestoreReport {
        w.service
            .apply_project_restore(
                prepared,
                &BTreeMap::new(),
                None,
                &AtomicBool::new(false),
                &quiet,
            )
            .unwrap()
    }

    #[test]
    fn memory_round_trips_into_the_bank_of_a_different_folder() {
        let w = world();
        let proj = w.tmp.path().join("proj");
        fs::create_dir_all(&proj).unwrap();
        let source = bank(&w, &proj);
        seed(
            &source,
            &[
                ("architecture.md", "arch\n"),
                ("product.md", "prod\n"),
                ("style.md", ""),
                ("state.json", "{\"v\":1}"),
                ("queue/s1/1.json", "{}"),
            ],
        );

        let sizes = measure(&w.root, &proj).unwrap();
        assert_eq!(
            sizes.memory,
            MemoryBank::Present {
                files: 2,
                bytes: 10
            }
        );
        assert_eq!(sizes.memory_dir, source);

        let out = w.backup(&proj, MEMORY, false).unwrap().expect("uploads");
        assert_eq!(out.backup.memory_bytes, Some(10));
        assert_eq!(out.backup.omp_bytes, None);
        assert_eq!(out.backup.file.props["mem"], "10");
        assert!(out.skipped.is_empty());
        let archive = only_archive(&w);
        let parts = members(&archive);
        let names: Vec<&str> = parts.iter().map(|m| m.0.as_str()).collect();
        assert_eq!(
            names,
            [
                "memory/architecture.md",
                "memory/product.md",
                "manifest.json"
            ],
            "empty files, state.json and queue/ are not memory"
        );
        let manifest: serde_json::Value = serde_json::from_slice(&parts[2].1).unwrap();
        assert_eq!(manifest["memory"]["files"].as_array().unwrap().len(), 2);
        assert!(manifest["omp"].is_null() && manifest["sessions"].is_null());

        // Same selection, nothing changed → skipped. Without the part there is nothing to save.
        assert!(w.backup(&proj, MEMORY, false).unwrap().is_none());
        assert!(matches!(
            w.backup(&proj, OMP, false),
            Err(Error::NothingToBackUp(_))
        ));

        // Restore into another folder: the files land in THAT folder's bank, not the source's.
        let backup = newest(&w);
        assert_eq!(backup.memory_bytes, Some(10));
        let target = w.tmp.path().join("elsewhere/target");
        let target_bank = bank(&w, &target);
        assert_ne!(target_bank, source);
        let prepared = w.prepare(&backup, &target, MEMORY, &w.root).unwrap();
        let plan = &prepared.plan;
        assert_eq!(plan.memory_dir, target_bank);
        assert!(!plan.memory_exists && plan.create_target);
        assert_eq!(plan.memory_added, ["architecture.md", "product.md"]);
        assert!(plan.memory_overwritten.is_empty() && plan.memory_skipped.is_empty());
        assert!(!target_bank.exists(), "prepare touches nothing");
        let report = apply(&w, prepared);
        assert_eq!((report.memory_written, report.memory_overwritten), (2, 0));
        assert!(report.memory_error.is_none() && report.memory_skipped.is_empty());
        assert!(report.safety_dir.is_none() && !report.cancelled);
        assert_eq!(read(&target_bank.join("architecture.md")), "arch\n");
        assert_eq!(read(&target_bank.join("product.md")), "prod\n");
        for absent in ["style.md", "state.json", "queue"] {
            assert!(!target_bank.join(absent).exists(), "{absent}");
        }
        assert_eq!(
            read(&source.join("product.md")),
            "prod\n",
            "source untouched"
        );

        // A restore prepared for another store writes under that store's memories folder.
        let other = SessionsRoot::at(w.tmp.path().join("other-agent/sessions"));
        let prepared = w.prepare(&backup, &target, MEMORY, &other).unwrap();
        assert_eq!(
            prepared.plan.memory_dir,
            sharpshooter::bank_dir(&other, &target)
        );
        assert!(
            prepared
                .plan
                .memory_dir
                .starts_with(w.tmp.path().join("other-agent/memories/sharpshooter"))
        );
        assert!(!prepared.plan.memory_exists);

        // Editing a memory file is a change.
        write(&source.join("style.md"), "now has content\n");
        assert!(w.backup(&proj, MEMORY, false).unwrap().is_some());
    }

    #[test]
    fn a_bank_without_non_empty_memory_files_is_nothing_to_back_up() {
        let w = world();
        let proj = w.tmp.path().join("proj");
        fs::create_dir_all(&proj).unwrap();
        assert!(matches!(
            w.backup(&proj, MEMORY, true),
            Err(Error::NothingToBackUp(_))
        ));
        seed(
            &bank(&w, &proj),
            &[("architecture.md", ""), ("state.json", "{}")],
        );
        assert!(matches!(
            w.backup(&proj, MEMORY, true),
            Err(Error::NothingToBackUp(_))
        ));
        assert_eq!(measure(&w.root, &proj).unwrap().memory, MemoryBank::Missing);
        assert!(w.mock.lock().unwrap().files.is_empty());
    }

    #[test]
    fn restoring_over_an_existing_bank_copies_replaced_files_first_and_keeps_the_rest() {
        let w = world();
        let proj = w.tmp.path().join("proj");
        write(&proj.join(".omp/AGENTS.md"), "remote rules\n");
        seed(
            &bank(&w, &proj),
            &[
                ("architecture.md", "new arch\n"),
                ("product.md", "new prod\n"),
            ],
        );
        w.backup(&proj, OMP_MEMORY, false).unwrap().unwrap();
        let backup = newest(&w);

        let target = w.tmp.path().join("target");
        write(&target.join(".omp/AGENTS.md"), "local rules\n");
        let tb = bank(&w, &target);
        let state = "{\"v\":1,\"lastConsolidatedAt\":5}";
        seed(
            &tb,
            &[
                ("architecture.md", "old arch\n"),
                ("product.md", ""),
                ("style.md", "my style\n"),
                ("state.json", state),
                ("queue/s1/1.json", "{\"q\":1}"),
            ],
        );
        let prepared = w.prepare(&backup, &target, OMP_MEMORY, &w.root).unwrap();
        let plan = &prepared.plan;
        assert!(plan.memory_exists);
        assert_eq!(plan.memory_overwritten, ["architecture.md"]);
        assert_eq!(
            plan.memory_added,
            ["product.md"],
            "an empty file has nothing to lose"
        );

        let stages = Mutex::new(Vec::new());
        let record = |p: Progress| {
            let mut s = stages.lock().unwrap();
            if s.last() != Some(&p.stage) {
                s.push(p.stage);
            }
        };
        let report = w
            .service
            .apply_project_restore(
                prepared,
                &BTreeMap::new(),
                None,
                &AtomicBool::new(false),
                &record,
            )
            .unwrap();
        assert_eq!((report.omp_written, report.omp_overwritten), (1, 1));
        assert_eq!((report.memory_written, report.memory_overwritten), (2, 1));
        assert!(report.memory_error.is_none());
        let stages = stages.into_inner().unwrap();
        let at = |s: Stage| stages.iter().position(|x| *x == s).unwrap();
        assert!(
            at(Stage::RestoringOmp) < at(Stage::RestoringMemory),
            "{stages:?}"
        );

        // One safety directory holds both parts' previous files.
        let safety = report.safety_dir.expect("something was overwritten");
        assert_eq!(read(&safety.join("AGENTS.md")), "local rules\n");
        assert_eq!(read(&safety.join("memory/architecture.md")), "old arch\n");
        assert!(!safety.join("memory/product.md").exists());

        assert_eq!(read(&tb.join("architecture.md")), "new arch\n");
        assert_eq!(read(&tb.join("product.md")), "new prod\n");
        assert_eq!(read(&tb.join("style.md")), "my style\n", "never deleted");
        assert_eq!(read(&tb.join("state.json")), state);
        assert_eq!(read(&tb.join("queue/s1/1.json")), "{\"q\":1}");
        let temps = fs::read_dir(&tb)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with('.'))
            .count();
        assert_eq!(temps, 0, "no temp files are left in omp's bank");
    }

    #[test]
    fn a_memory_only_restore_makes_its_own_safety_copy() {
        let w = world();
        let proj = w.tmp.path().join("proj");
        seed(&bank(&w, &proj), &[("style.md", "remote style\n")]);
        w.backup(&proj, MEMORY, false).unwrap().unwrap();
        let target = w.tmp.path().join("target");
        let tb = bank(&w, &target);
        seed(&tb, &[("style.md", "local style\n")]);
        let prepared = w.prepare(&newest(&w), &target, MEMORY, &w.root).unwrap();
        let report = apply(&w, prepared);
        let safety = report.safety_dir.unwrap();
        assert!(safety.starts_with(w.tmp.path().join("safety")));
        assert_eq!(read(&safety.join("memory/style.md")), "local style\n");
        assert_eq!(read(&tb.join("style.md")), "remote style\n");
    }

    /// Two files, restored while omp holds the lock, then once it lets go.
    #[test]
    fn a_held_bank_lock_leaves_the_memory_alone_and_a_released_one_is_waited_for() {
        let w = world();
        let proj = w.tmp.path().join("proj");
        write(&proj.join(".omp/AGENTS.md"), "rules\n");
        seed(&bank(&w, &proj), &[("architecture.md", "remote\n")]);
        w.backup(&proj, OMP_MEMORY, false).unwrap().unwrap();
        let backup = newest(&w);

        let target = w.tmp.path().join("target");
        let tb = bank(&w, &target);
        seed(&tb, &[("architecture.md", "local\n")]);
        let held = sharpshooter::lock_bank(&tb, Duration::ZERO)
            .unwrap()
            .expect("free");
        let prepared = w.prepare(&backup, &target, OMP_MEMORY, &w.root).unwrap();
        let report = apply(&w, prepared);
        assert_eq!(
            report.memory_error.as_deref(),
            Some("omp is updating this folder's memory right now; try again in a moment")
        );
        assert_eq!((report.memory_written, report.memory_overwritten), (0, 0));
        assert!(report.safety_dir.is_none() && !report.cancelled);
        assert_eq!(read(&tb.join("architecture.md")), "local\n", "untouched");
        assert_eq!(report.omp_written, 1, ".omp is restored regardless");
        assert_eq!(read(&target.join(".omp/AGENTS.md")), "rules\n");

        // omp finishes while we wait: the restore gets the lock and writes.
        let prepared = w.prepare(&backup, &target, OMP_MEMORY, &w.root).unwrap();
        let releaser = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            drop(held);
        });
        let report = apply(&w, prepared);
        releaser.join().unwrap();
        assert!(report.memory_error.is_none(), "{:?}", report.memory_error);
        assert_eq!((report.memory_written, report.memory_overwritten), (1, 1));
        assert_eq!(read(&tb.join("architecture.md")), "remote\n");
        let safety = report.safety_dir.unwrap();
        assert_eq!(read(&safety.join("memory/architecture.md")), "local\n");
    }

    /// Cancelling while omp holds the lock ends the restore as cancelled, not as "omp is busy".
    #[test]
    fn cancelling_while_waiting_for_the_bank_lock_is_a_cancel() {
        let w = world();
        let proj = w.tmp.path().join("proj");
        seed(&bank(&w, &proj), &[("architecture.md", "remote\n")]);
        w.backup(&proj, MEMORY, false).unwrap().unwrap();
        let target = w.tmp.path().join("target");
        let tb = bank(&w, &target);
        seed(&tb, &[("architecture.md", "local\n")]);
        let _held = sharpshooter::lock_bank(&tb, Duration::ZERO)
            .unwrap()
            .expect("free");
        let prepared = w.prepare(&newest(&w), &target, MEMORY, &w.root).unwrap();
        let cancel = AtomicBool::new(false);
        let started = std::time::Instant::now();
        let report = std::thread::scope(|s| {
            s.spawn(|| {
                std::thread::sleep(Duration::from_millis(250));
                cancel.store(true, std::sync::atomic::Ordering::Relaxed);
            });
            w.service
                .apply_project_restore(prepared, &BTreeMap::new(), None, &cancel, &quiet)
                .unwrap()
        });
        assert!(
            started.elapsed() < Duration::from_millis(1400),
            "did not wait out the lock"
        );
        assert!(report.cancelled);
        assert!(report.memory_error.is_none(), "{:?}", report.memory_error);
        assert_eq!((report.memory_written, report.memory_overwritten), (0, 0));
        assert!(report.safety_dir.is_none());
        assert_eq!(read(&tb.join("architecture.md")), "local\n", "untouched");
    }

    #[cfg(unix)]
    #[test]
    fn a_held_bank_lock_does_not_stop_the_sessions() {
        let w = world();
        let proj = w.tmp.path().join("proj");
        fs::create_dir_all(&proj).unwrap();
        session(&w.agent, "s1", &proj, "");
        seed(&bank(&w, &proj), &[("architecture.md", "remote\n")]);
        let all = Parts {
            omp: false,
            sessions: true,
            memory: true,
        };
        w.backup(&proj, all, false).unwrap().unwrap();
        let target = w.tmp.path().join("target");
        let tb = bank(&w, &target);
        let _held = sharpshooter::lock_bank(&tb, Duration::ZERO)
            .unwrap()
            .unwrap();
        let omp = fake_omp(w.tmp.path(), "ok.sh", OK_OMP);
        let prepared = w.prepare(&newest(&w), &target, all, &w.root).unwrap();
        let keep_both = keep_both(&prepared);
        let report = w
            .service
            .apply_project_restore(
                prepared,
                &keep_both,
                Some(&omp),
                &AtomicBool::new(false),
                &quiet,
            )
            .unwrap();
        assert!(report.memory_error.is_some() && report.memory_written == 0);
        assert_eq!(report.sessions_added.len(), 1);
        assert!(report.sessions_failed.is_empty());
        assert!(!tb.join("architecture.md").exists());
    }

    #[test]
    fn cancelling_between_memory_files_keeps_the_first() {
        let w = world();
        let proj = w.tmp.path().join("proj");
        seed(
            &bank(&w, &proj),
            &[
                ("architecture.md", "a\n"),
                ("product.md", "p\n"),
                ("style.md", "s\n"),
            ],
        );
        w.backup(&proj, MEMORY, false).unwrap().unwrap();
        let target = w.tmp.path().join("target");
        let prepared = w.prepare(&newest(&w), &target, MEMORY, &w.root).unwrap();
        let cancel = AtomicBool::new(false);
        let report = w
            .service
            .apply_project_restore(prepared, &BTreeMap::new(), None, &cancel, &|p: Progress| {
                if p.stage == Stage::RestoringMemory && p.done == 1 {
                    cancel.store(true, std::sync::atomic::Ordering::Relaxed);
                }
            })
            .unwrap();
        assert!(report.cancelled);
        assert_eq!(report.memory_written, 1);
        let tb = bank(&w, &target);
        assert_eq!(read(&tb.join("architecture.md")), "a\n");
        assert!(!tb.join("product.md").exists());
    }

    #[cfg(unix)]
    #[test]
    fn restore_never_writes_through_a_symlinked_bank_or_memory_file() {
        let w = world();
        let proj = w.tmp.path().join("proj");
        seed(
            &bank(&w, &proj),
            &[
                ("architecture.md", "a\n"),
                ("product.md", "p\n"),
                ("style.md", "s\n"),
            ],
        );
        w.backup(&proj, MEMORY, false).unwrap().unwrap();
        let backup = newest(&w);

        // A symlinked memory file in the target bank.
        let target = w.tmp.path().join("t1");
        let tb = bank(&w, &target);
        let outside = w.tmp.path().join("outside.md");
        fs::write(&outside, "keep me").unwrap();
        seed(&tb, &[("architecture.md", "old\n")]);
        symlink(&outside, tb.join("product.md")).unwrap();
        let prepared = w.prepare(&backup, &target, MEMORY, &w.root).unwrap();
        assert_eq!(
            prepared.plan.memory_skipped,
            ["product.md: a symbolic link is in the way"]
        );
        assert_eq!(prepared.plan.memory_overwritten, ["architecture.md"]);
        assert_eq!(prepared.plan.memory_added, ["style.md"]);
        let report = apply(&w, prepared);
        assert_eq!(
            report.memory_skipped,
            ["product.md: a symbolic link is in the way"]
        );
        assert_eq!((report.memory_written, report.memory_overwritten), (2, 1));
        assert_eq!(fs::read_to_string(&outside).unwrap(), "keep me");
        assert_eq!(read(&tb.join("architecture.md")), "a\n");

        // A symlinked bank folder: nothing is written into what it points to.
        let target = w.tmp.path().join("t2");
        let tb = bank(&w, &target);
        let real = w.tmp.path().join("real-bank");
        write(&real.join("keep.md"), "x");
        fs::create_dir_all(tb.parent().unwrap()).unwrap();
        symlink(&real, &tb).unwrap();
        let prepared = w.prepare(&backup, &target, MEMORY, &w.root).unwrap();
        let plan = &prepared.plan;
        assert!(!plan.memory_exists);
        assert!(plan.memory_added.is_empty() && plan.memory_overwritten.is_empty());
        assert_eq!(plan.memory_skipped.len(), 3);
        assert!(
            plan.memory_skipped
                .iter()
                .all(|s| s.ends_with(": the memory folder is a symbolic link"))
        );
        let report = apply(&w, prepared);
        assert_eq!(report.memory_written, 0);
        assert_eq!(report.memory_skipped.len(), 3);
        assert!(report.memory_error.is_none());
        let names: Vec<_> = fs::read_dir(&real).unwrap().flatten().collect();
        assert_eq!(names.len(), 1, "only keep.md");
    }

    /// A manifest with only a memory part.
    fn memory_manifest(files: &[(&str, &[u8])]) -> serde_json::Value {
        let mut m = manifest_for(&[]);
        m["omp"] = serde_json::Value::Null;
        m["memory"] = serde_json::json!({ "files": manifest_for(files)["omp"]["files"] });
        m
    }

    #[test]
    fn memory_archive_members_are_limited_to_the_three_names() {
        let w = world();
        let target = w.tmp.path().join("target");
        let file = tar::EntryType::Regular;
        let json = |m: &serde_json::Value| m.to_string().into_bytes();
        let invalid = |r: Result<PreparedProjectRestore>| matches!(r, Err(Error::InvalidBackup(_)));
        let try_restore = |entries: &[(&str, tar::EntryType, &[u8])], parts: Parts| {
            let b = put_backup(&w, crafted(entries));
            w.prepare(&b, &target, parts, &w.root)
        };

        // Control.
        let good = memory_manifest(&[("style.md", b"style")]);
        let prepared = try_restore(
            &[
                ("memory/style.md", file, b"style"),
                ("manifest.json", file, &json(&good)),
            ],
            MEMORY,
        )
        .unwrap();
        assert_eq!(prepared.plan.memory_added, ["style.md"]);

        // Other names, nested names, unlisted or corrupt members, a directory entry.
        for (name, m) in [
            ("memory/evil.md", memory_manifest(&[("evil.md", b"x")])),
            (
                "memory/sub/style.md",
                memory_manifest(&[("sub/style.md", b"x")]),
            ),
            (
                "memory/state.json",
                memory_manifest(&[("state.json", b"x")]),
            ),
            (
                "memory/../style.md",
                memory_manifest(&[("../style.md", b"x")]),
            ),
        ] {
            let r = try_restore(
                &[(name, file, b"x"), ("manifest.json", file, &json(&m))],
                MEMORY,
            );
            assert!(invalid(r), "{name}");
        }
        let unlisted = manifest_for(&[]);
        assert!(invalid(try_restore(
            &[
                ("memory/style.md", file, b"style"),
                ("manifest.json", file, &json(&unlisted)),
            ],
            MEMORY
        )));
        let corrupt = memory_manifest(&[("style.md", b"other")]);
        assert!(invalid(try_restore(
            &[
                ("memory/style.md", file, b"style"),
                ("manifest.json", file, &json(&corrupt)),
            ],
            MEMORY
        )));
        assert!(invalid(try_restore(
            &[
                ("memory/style.md", tar::EntryType::Symlink, b""),
                ("manifest.json", file, &json(&good)),
            ],
            MEMORY
        )));
    }

    #[test]
    fn backups_made_before_memory_existed_still_restore_but_have_no_memory() {
        let w = world();
        let target = w.tmp.path().join("target");
        let file = tar::EntryType::Regular;
        // No "memory" key at all, like every manifest written by the earlier builds.
        let old = manifest_for(&[("a.md", b"alpha")]);
        assert!(old.get("memory").is_none());
        let archive = crafted(&[
            ("omp/a.md", file, b"alpha"),
            ("manifest.json", file, old.to_string().as_bytes()),
        ]);
        let backup = put_backup(&w, archive);
        assert_eq!(backup.memory_bytes, None);
        let prepared = w.prepare(&backup, &target, OMP, &w.root).unwrap();
        assert!(prepared.plan.memory_added.is_empty() && !prepared.plan.memory_exists);
        assert!(prepared.plan.memory_dir.as_os_str().is_empty());
        let report = apply(&w, prepared);
        assert_eq!((report.omp_written, report.memory_written), (1, 0));
        assert!(report.memory_error.is_none());
        assert!(matches!(
            w.prepare(&backup, &target, MEMORY, &w.root),
            Err(Error::InvalidBackup(_))
        ));
        assert!(matches!(
            w.prepare(&backup, &target, OMP_MEMORY, &w.root),
            Err(Error::InvalidBackup(_))
        ));
    }
}
