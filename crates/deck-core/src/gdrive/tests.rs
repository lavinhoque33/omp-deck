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

// ---------------------------------------------------------------- project sync

mod project_sync {
    use std::collections::BTreeSet;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::symlink;
    use std::path::PathBuf;

    use sha2::{Digest, Sha256};

    use super::*;
    use crate::gdrive::archive::hex;
    use crate::omp::Omp;
    use crate::sessions::{self, SessionsRoot};

    const BOTH: Parts = Parts {
        omp: true,
        sessions: true,
    };
    const OMP: Parts = Parts {
        omp: true,
        sessions: false,
    };
    const SESSIONS: Parts = Parts {
        omp: false,
        sessions: true,
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
        let enc = cwd.display().to_string().replace('/', "-");
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
        fs::set_permissions(
            proj.join(".omp/hooks/pre/run.sh"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        write(&proj.join(".omp/node_modules/dep/x.js"), "dep");
        write(&proj.join(".omp/.git/HEAD"), "ref");
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
        assert_eq!(
            fs::metadata(&state_file).unwrap().permissions().mode() & 0o777,
            0o600
        );
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
            .apply_project_restore(prepared, None, &AtomicBool::new(false), &quiet)
            .unwrap();
        assert_eq!((report.omp_written, report.omp_overwritten), (3, 1));
        assert_eq!(read(&target.join(".omp/AGENTS.md")), "remote rules\n");
        assert_eq!(read(&target.join(".omp/config.yml")), "a: 1\n");
        assert_eq!(read(&target.join(".omp/local-only.md")), "mine\n");
        let mode = fs::metadata(target.join(".omp/hooks/pre/run.sh"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o111, 0o111, "exec bit kept");
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
            .apply_project_restore(prepared, None, &AtomicBool::new(false), &quiet)
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
            .backup_project(&w.root, &proj, OMP, true, &AtomicBool::new(true), &quiet)
            .unwrap_err();
        assert!(matches!(err, Error::Cancelled), "{err}");
        assert!(w.mock.lock().unwrap().files.is_empty());
        assert_eq!(w.staging(), 0);
        assert!(w.service.project_state().projects.is_empty());
    }

    #[test]
    fn stale_staging_directories_are_swept_at_job_start() {
        let w = world();
        let cache = w.tmp.path().join("cache");
        let (stale, fresh) = (cache.join("stale"), cache.join("fresh"));
        write(&stale.join("leftover.tar.gz"), "partial");
        write(&fresh.join("running.tar.gz"), "in use");
        let two_days_ago = std::time::SystemTime::now() - Duration::from_secs(48 * 3600);
        fs::File::open(&stale)
            .unwrap()
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

    /// A sandbox for the real `omp`: its own agent dir, HOME and config dir. `None` (with a
    /// message) when omp is not installed.
    fn real_omp(base: &Path, name: &str) -> Option<(Omp, PathBuf)> {
        let Some(omp) = Omp::detect() else {
            eprintln!("omp not found on PATH; skipping the session restore test");
            return None;
        };
        let base = base.join(name);
        let (agent, home, config) = (base.join("agent"), base.join("home"), base.join("config"));
        for d in [&agent, &home, &config] {
            fs::create_dir_all(d).unwrap();
        }
        link_natives(&home);
        let omp = omp
            .with_env("PI_CODING_AGENT_DIR", &agent)
            .with_env("PI_CONFIG_DIR", &config)
            .with_env("HOME", &home);
        Some((omp, agent))
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
        assert!(prepared.plan.sessions[0].new_id);
        let report = w
            .service
            .apply_project_restore(prepared, Some(&omp), &AtomicBool::new(false), &quiet)
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
        assert!(!prepared.plan.sessions[0].new_id);
        let report = w
            .service
            .apply_project_restore(prepared, Some(&omp2), &AtomicBool::new(false), &quiet)
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
                .apply_project_restore(prepared, None, &AtomicBool::new(false), &quiet)
                .is_err()
        );
    }

    // ------------------------------------------------------------ links, skips, conflicts

    #[test]
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
            .apply_project_restore(prepared, None, &AtomicBool::new(false), &quiet)
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
            .backup_project(&w.root, &proj, OMP, true, &AtomicBool::new(false), &record)
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

    fn fake_omp(dir: &Path, name: &str, body: &str) -> Omp {
        let path = dir.join(name);
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        Omp::at(path)
    }

    /// A project with `ids` sessions and `files` in `.omp`, backed up; its list entry.
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
        };
        w.backup(&proj, parts, false).unwrap().unwrap();
        let backup = w.service.list_projects().unwrap().remove(0);
        (proj, backup)
    }

    const OK_OMP: &str = "echo /store/sessions/x/new.jsonl; echo 'Unpacked session (abc).' >&2";

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
            .apply_project_restore(prepared, Some(&omp), &AtomicBool::new(false), &quiet)
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
        let report = w
            .service
            .apply_project_restore(prepared, Some(&omp), &AtomicBool::new(false), &quiet)
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

    /// A progress callback that cancels once `stage` has finished its first item.
    fn stop_after(stage: Stage, cancel: &AtomicBool) -> impl Fn(Progress) + Sync + '_ {
        move |p: Progress| {
            if p.stage == stage && p.done == 1 {
                cancel.store(true, std::sync::atomic::Ordering::Relaxed);
            }
        }
    }

    #[test]
    fn cancelling_keeps_what_was_already_restored() {
        let w = world();
        let (_, backup) = backed_up(&w, &["s1", "s2"], &["a.md", "b.md"]);
        let omp = fake_omp(w.tmp.path(), "ok.sh", OK_OMP);
        let both = Parts {
            omp: true,
            sessions: true,
        };

        // Between the two sessions: the first is kept in the report.
        let cancel = AtomicBool::new(false);
        let target = w.tmp.path().join("t1");
        let prepared = w.prepare(&backup, &target, both, &w.root).unwrap();
        let report = w
            .service
            .apply_project_restore(
                prepared,
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
            .apply_project_restore(prepared, Some(&omp), &AtomicBool::new(true), &quiet)
            .unwrap_err();
        assert_eq!(err.to_string(), "cancelled");
        assert!(!target.exists());
    }
}
