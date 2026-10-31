//! The relay serving the phone app (`--web-dir`) next to its WebSocket routes, and the
//! per-request HTTP deadlines: real sockets on 127.0.0.1, raw HTTP/1.1.

use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;

use deck_relay::{Config, RelayError, rooms};
use deck_remote::relay::{GATEWAY_PATH, HELLO_OK, Hello};
use futures_util::{SinkExt as _, StreamExt as _};
use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::TcpStream;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep, timeout};
use tokio_tungstenite::tungstenite::Message;

const WAIT: Duration = Duration::from_secs(5);

const INDEX: &[u8] =
    b"<!doctype html><title>OmpDeck</title><script src=\"/assets/app-1a2b3c.js\"></script>";
const APP_JS: &[u8] = b"console.log('app');";
const APP_JS_GZ: &[u8] = b"\x1f\x8bnot-really-gzip-but-distinct";
const MANIFEST: &[u8] = br#"{"id":"/","scope":"/"}"#;

const CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self' 'unsafe-inline'; \
                   img-src 'self' data: blob:; font-src 'self'; connect-src 'self'; \
                   manifest-src 'self'; worker-src 'self'; base-uri 'none'; form-action 'none'; \
                   frame-ancestors 'none'";

/// A built-app lookalike: hashed assets with a gz sibling, the unhashed entry points, a dotfile.
fn web_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path();
    std::fs::create_dir(p.join("assets")).unwrap();
    std::fs::write(p.join("index.html"), INDEX).unwrap();
    std::fs::write(
        p.join("sw.js"),
        b"self.addEventListener('fetch', () => {});",
    )
    .unwrap();
    std::fs::write(p.join("manifest.webmanifest"), MANIFEST).unwrap();
    std::fs::write(p.join("icon-192.png"), b"\x89PNG\r\n\x1a\n").unwrap();
    std::fs::write(p.join("assets/app-1a2b3c.js"), APP_JS).unwrap();
    std::fs::write(p.join("assets/app-1a2b3c.js.gz"), APP_JS_GZ).unwrap();
    std::fs::write(p.join(".env"), b"SECRET=1").unwrap();
    dir
}

struct Relay {
    addr: SocketAddr,
    _state: tempfile::TempDir,
    key: deck_remote::relay::RoomKey,
    stop: Option<oneshot::Sender<()>>,
    task: JoinHandle<Result<(), RelayError>>,
}

impl Relay {
    async fn start(web: Option<&Path>, tweak: impl FnOnce(&mut Config)) -> Relay {
        let state = tempfile::tempdir().unwrap();
        let (key, _) = rooms::enroll(state.path(), "test").unwrap();
        let mut config = Config::new(state.path());
        config.close_timeout = Duration::from_secs(1);
        config.web_dir = web.map(Path::to_path_buf);
        tweak(&mut config);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (stop, stopped) = oneshot::channel::<()>();
        let task = tokio::spawn(deck_relay::serve(config, listener, async move {
            let _ = stopped.await;
        }));
        Relay {
            addr,
            _state: state,
            key,
            stop: Some(stop),
            task,
        }
    }

    async fn conn(&self) -> Conn {
        Conn(BufReader::new(TcpStream::connect(self.addr).await.unwrap()))
    }

    async fn get(&self, path: &str) -> Resp {
        self.conn().await.request("GET", path, &[]).await
    }

    async fn stop(mut self) {
        let _ = self.stop.take().unwrap().send(());
        timeout(Duration::from_secs(6), &mut self.task)
            .await
            .expect("serve returns within the shutdown timeout")
            .unwrap()
            .unwrap();
    }
}

#[derive(Debug)]
struct Resp {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Resp {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// One keep-alive HTTP/1.1 connection.
struct Conn(BufReader<TcpStream>);

impl Conn {
    async fn request(&mut self, method: &str, path: &str, headers: &[(&str, &str)]) -> Resp {
        let mut req = format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\n");
        for (n, v) in headers {
            req.push_str(&format!("{n}: {v}\r\n"));
        }
        req.push_str("\r\n");
        self.0.get_mut().write_all(req.as_bytes()).await.unwrap();
        timeout(WAIT, self.response(method == "HEAD"))
            .await
            .expect("response in time")
    }

    async fn response(&mut self, head: bool) -> Resp {
        let mut line = String::new();
        self.0.read_line(&mut line).await.unwrap();
        let status = line
            .split(' ')
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or_else(|| panic!("bad status line {line:?}"));
        let mut headers = Vec::new();
        loop {
            line.clear();
            self.0.read_line(&mut line).await.unwrap();
            let l = line.trim_end();
            if l.is_empty() {
                break;
            }
            let (n, v) = l.split_once(':').unwrap();
            headers.push((n.trim().to_owned(), v.trim().to_owned()));
        }
        let mut resp = Resp {
            status,
            headers,
            body: Vec::new(),
        };
        let len: usize = resp
            .header("content-length")
            .map_or(0, |v| v.parse().unwrap());
        if !head && status != 304 {
            resp.body = vec![0; len];
            self.0.read_exact(&mut resp.body).await.unwrap();
        }
        resp
    }

    /// The relay closes the connection within [`WAIT`].
    async fn closed(mut self) {
        let mut buf = [0u8; 64];
        match timeout(WAIT, self.0.read(&mut buf)).await {
            Ok(Ok(0) | Err(_)) => {}
            Ok(Ok(n)) => panic!("expected a closed socket, read {n} bytes"),
            Err(_) => panic!("socket still open after {WAIT:?}"),
        }
    }
}

fn assert_security_headers(r: &Resp) {
    assert_eq!(r.header("content-security-policy"), Some(CSP), "{r:?}");
    assert_eq!(r.header("referrer-policy"), Some("no-referrer"));
    assert_eq!(r.header("x-content-type-options"), Some("nosniff"));
    assert_eq!(r.header("cross-origin-opener-policy"), Some("same-origin"));
    assert_eq!(
        r.header("cross-origin-resource-policy"),
        Some("same-origin")
    );
    assert_eq!(r.header("x-frame-options"), Some("DENY"));
    assert_eq!(
        r.header("permissions-policy"),
        Some("camera=(), microphone=(), geolocation=(), payment=(), usb=()")
    );
    // Plain HTTP (behind a TLS proxy): HSTS is the proxy's business.
    assert_eq!(r.header("strict-transport-security"), None);
}

#[tokio::test]
async fn serves_the_app_with_security_headers() {
    let web = web_dir();
    let r = Relay::start(Some(web.path()), |_| {}).await;
    for path in ["/", "/index.html", "/?source=pwa"] {
        let resp = r.get(path).await;
        assert_eq!(resp.status, 200, "{path}");
        assert_eq!(resp.body, INDEX);
        assert_eq!(
            resp.header("content-type"),
            Some("text/html; charset=utf-8")
        );
        assert_eq!(resp.header("cache-control"), Some("no-cache"));
        assert_security_headers(&resp);
    }
    let mut c = r.conn().await;
    let head = c.request("HEAD", "/", &[]).await;
    assert_eq!(head.status, 200);
    assert_eq!(
        head.header("content-length"),
        Some(INDEX.len().to_string().as_str())
    );
    assert_security_headers(&head);
    // The connection carries on after a HEAD (no stray body bytes).
    assert_eq!(c.request("GET", "/sw.js", &[]).await.status, 200);
    r.stop().await;
}

#[tokio::test]
async fn cache_rules_and_content_types() {
    let web = web_dir();
    let r = Relay::start(Some(web.path()), |_| {}).await;
    let cases = [
        (
            "/assets/app-1a2b3c.js",
            "text/javascript; charset=utf-8",
            "public, max-age=31536000, immutable",
        ),
        ("/sw.js", "text/javascript; charset=utf-8", "no-cache"),
        (
            "/manifest.webmanifest",
            "application/manifest+json",
            "no-cache",
        ),
        ("/icon-192.png", "image/png", "no-cache"),
    ];
    for (path, content_type, cache) in cases {
        let resp = r.get(path).await;
        assert_eq!(resp.status, 200, "{path}");
        assert_eq!(resp.header("content-type"), Some(content_type), "{path}");
        assert_eq!(resp.header("cache-control"), Some(cache), "{path}");
        assert_security_headers(&resp);
    }
    assert_eq!(r.get("/manifest.webmanifest").await.body, MANIFEST);
    r.stop().await;
}

#[tokio::test]
async fn unknown_paths_v1_and_traversal_are_not_found() {
    let web = web_dir();
    let r = Relay::start(Some(web.path()), |_| {}).await;
    for path in [
        "/nope",
        "/nope/",
        "/assets/",
        "/v1/x",
        "/v1/index.html",
        "/v1",
        "/../index.html",
        "/assets/../index.html",
        "/%2e%2e/index.html",
        "/.env",
        "/assets/app-1a2b3c.js.gz",
    ] {
        let resp = r.get(path).await;
        assert_eq!(resp.status, 404, "{path}");
        assert_security_headers(&resp);
    }
    // The explicit routes win over static files.
    let health = r.get("/healthz").await;
    assert_eq!((health.status, health.body.as_slice()), (200, &b"ok"[..]));
    assert_ne!(r.get(GATEWAY_PATH).await.status, 200);
    r.stop().await;
}

#[tokio::test]
async fn other_methods_answer_405() {
    let web = web_dir();
    let r = Relay::start(Some(web.path()), |_| {}).await;
    for method in ["POST", "PUT", "DELETE", "OPTIONS"] {
        let resp = r
            .conn()
            .await
            .request(method, "/index.html", &[("Content-Length", "0")])
            .await;
        assert_eq!(resp.status, 405, "{method}");
        assert_eq!(resp.header("allow"), Some("GET, HEAD"));
        assert_security_headers(&resp);
    }
    r.stop().await;
}

#[tokio::test]
async fn etags_revalidate_with_304() {
    let web = web_dir();
    let r = Relay::start(Some(web.path()), |_| {}).await;
    let first = r.get("/").await;
    let etag = first.header("etag").unwrap().to_owned();
    assert!(etag.starts_with('"') && etag.ends_with('"'), "{etag}");
    let mut c = r.conn().await;
    let again = c
        .request("GET", "/", &[("If-None-Match", etag.as_str())])
        .await;
    assert_eq!(again.status, 304);
    assert!(again.body.is_empty());
    assert_eq!(again.header("etag"), Some(etag.as_str()));
    assert_eq!(again.header("cache-control"), Some("no-cache"));
    assert_security_headers(&again);
    let weak = format!("\"other\", W/{etag}");
    let weak = c.request("GET", "/", &[("If-None-Match", &weak)]).await;
    assert_eq!(weak.status, 304);
    let changed = c
        .request("GET", "/", &[("If-None-Match", "\"stale\"")])
        .await;
    assert_eq!((changed.status, changed.body.as_slice()), (200, INDEX));
    // Another file has another tag.
    assert_ne!(r.get("/sw.js").await.header("etag"), Some(etag.as_str()));
    r.stop().await;
}

#[tokio::test]
async fn gzip_sibling_is_served_only_when_accepted() {
    let web = web_dir();
    let r = Relay::start(Some(web.path()), |_| {}).await;
    let path = "/assets/app-1a2b3c.js";
    let plain = r.get(path).await;
    assert_eq!(plain.body, APP_JS);
    assert_eq!(plain.header("content-encoding"), None);
    assert_eq!(plain.header("vary"), Some("Accept-Encoding"));
    let mut c = r.conn().await;
    let gz = c
        .request("GET", path, &[("Accept-Encoding", "gzip, deflate, br")])
        .await;
    assert_eq!(gz.body, APP_JS_GZ);
    assert_eq!(gz.header("content-encoding"), Some("gzip"));
    assert_eq!(gz.header("vary"), Some("Accept-Encoding"));
    assert_eq!(
        gz.header("content-type"),
        Some("text/javascript; charset=utf-8")
    );
    assert_ne!(gz.header("etag"), plain.header("etag"));
    let refused = c
        .request("GET", path, &[("Accept-Encoding", "gzip;q=0, identity")])
        .await;
    assert_eq!(refused.body, APP_JS);
    assert_eq!(refused.header("content-encoding"), None);
    // No brotli sibling: `br` alone gets the identity body.
    let br = c.request("GET", path, &[("Accept-Encoding", "br")]).await;
    assert_eq!(br.body, APP_JS);
    r.stop().await;
}

#[tokio::test]
async fn keep_alive_outlives_the_header_timeout() {
    let web = web_dir();
    let r = Relay::start(Some(web.path()), |_| {}).await;
    let mut c = r.conn().await;
    let started = Instant::now();
    for i in 0..3u32 {
        sleep(Duration::from_secs(6) * i.min(1)).await;
        let resp = c.request("GET", "/", &[]).await;
        assert_eq!(resp.status, 200, "request {i}");
        assert_eq!(resp.body, INDEX);
    }
    assert!(started.elapsed() >= Duration::from_secs(12));
    r.stop().await;
}

#[tokio::test]
async fn the_header_deadline_still_closes_silent_and_slow_connections() {
    let web = web_dir();
    let r = Relay::start(Some(web.path()), |c| {
        c.header_timeout = Duration::from_millis(300);
    })
    .await;
    let started = Instant::now();
    // Never sends a request.
    r.conn().await.closed().await;
    assert!(started.elapsed() >= Duration::from_millis(300));
    // Keep-alive: the first request is answered, the next one starts and stalls mid-headers.
    let mut c = r.conn().await;
    assert_eq!(c.request("GET", "/", &[]).await.status, 200);
    sleep(Duration::from_millis(500)).await;
    let stalled = Instant::now();
    c.0.get_mut()
        .write_all(b"GET / HTTP/1.1\r\nHost: x\r\n")
        .await
        .unwrap();
    c.closed().await;
    assert!(stalled.elapsed() >= Duration::from_millis(250));
    r.stop().await;
}

#[tokio::test]
async fn idle_keep_alive_is_closed() {
    let web = web_dir();
    let r = Relay::start(Some(web.path()), |c| {
        c.keep_alive_timeout = Duration::from_millis(400);
    })
    .await;
    let mut c = r.conn().await;
    assert_eq!(c.request("GET", "/", &[]).await.status, 200);
    let idle = Instant::now();
    c.closed().await;
    assert!(idle.elapsed() >= Duration::from_millis(350));
    r.stop().await;
}

#[tokio::test]
async fn the_last_allowed_request_closes_the_connection() {
    let web = web_dir();
    let r = Relay::start(Some(web.path()), |c| c.max_requests_per_connection = 2).await;
    let mut c = r.conn().await;
    let first = c.request("GET", "/", &[]).await;
    assert_eq!(first.header("connection"), None);
    let second = c.request("GET", "/", &[]).await;
    assert_eq!(second.status, 200);
    assert_eq!(second.header("connection"), Some("close"));
    c.closed().await;
    r.stop().await;
}

#[tokio::test]
async fn websockets_work_alongside_the_app() {
    let web = web_dir();
    let r = Relay::start(Some(web.path()), |c| {
        c.header_timeout = Duration::from_millis(300);
        c.keep_alive_timeout = Duration::from_millis(300);
    })
    .await;
    assert_eq!(r.get("/").await.status, 200);
    let tcp = TcpStream::connect(r.addr).await.unwrap();
    let (mut ws, _) =
        tokio_tungstenite::client_async(format!("ws://{}{GATEWAY_PATH}", r.addr), tcp)
            .await
            .unwrap();
    let hello = Hello::Gateway {
        room: r.key.room,
        secret: r.key.secret,
        epoch: 0,
    };
    ws.send(Message::Binary(hello.encode().into()))
        .await
        .unwrap();
    let ok = loop {
        match timeout(WAIT, ws.next()).await.expect("hello reply in time") {
            Some(Ok(Message::Binary(b))) => break b.to_vec(),
            Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
            other => panic!("expected HELLO_OK, got {other:?}"),
        }
    };
    assert_eq!(ok, HELLO_OK);
    // The upgraded socket outlives the HTTP deadlines: still open, nothing to read.
    sleep(Duration::from_millis(800)).await;
    assert!(
        timeout(Duration::from_millis(300), ws.next())
            .await
            .is_err(),
        "the WebSocket was closed"
    );
    assert_eq!(r.get("/sw.js").await.status, 200);
    r.stop().await;
}

#[tokio::test]
async fn no_web_dir_serves_no_app() {
    let r = Relay::start(None, |_| {}).await;
    for path in ["/", "/index.html", "/sw.js"] {
        let resp = r.get(path).await;
        assert_eq!(resp.status, 404, "{path}");
        assert_eq!(resp.header("content-security-policy"), None);
    }
    assert_eq!(r.get("/healthz").await.status, 200);
    r.stop().await;
}

#[tokio::test]
async fn a_missing_web_dir_fails_startup() {
    let missing = tempfile::tempdir().unwrap();
    let path = missing.path().join("absent");
    let r = Relay::start(Some(&path), |_| {}).await;
    let result = timeout(WAIT, r.task).await.unwrap().unwrap();
    assert!(matches!(result, Err(RelayError::Io(..))), "{result:?}");
}
