//! The FCM sender against a loopback "Google": credentials, the OAuth assertion, the envelope a
//! phone decrypts, and what each kind of answer does (retry, drop, or clear a registration).

use std::collections::HashMap;
use std::fmt::Write as _;
use std::io::{BufRead as _, BufReader, Write as _};
use std::net::TcpListener;
use std::sync::mpsc;

use ring::aead::{AES_256_GCM, Aad, LessSafeKey, Nonce, UnboundKey};
use ring::signature::{RSA_PKCS1_2048_8192_SHA256, UnparsedPublicKey};

use super::*;

/// A throwaway key made for these tests; it belongs to no account.
const TEST_KEY_PEM: &str = include_str!("test-rsa-key.pem");
pub(crate) const DESKTOP: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";
pub(crate) const PROJECT: &str = "ompdeck-test";

fn b64(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

fn unb64(text: &str) -> Vec<u8> {
    URL_SAFE_NO_PAD.decode(text).unwrap()
}

fn account(edit: impl FnOnce(&mut serde_json::Value)) -> Vec<u8> {
    let mut v = serde_json::json!({
        "type": "service_account",
        "project_id": PROJECT,
        "client_email": "gateway@ompdeck-test.iam.gserviceaccount.com",
        "private_key": TEST_KEY_PEM,
        "token_uri": "https://evil.example/token",
    });
    edit(&mut v);
    serde_json::to_vec(&v).unwrap()
}

pub(crate) fn creds() -> Credentials {
    Credentials::parse(&account(|_| {})).unwrap()
}

/// What a phone does with the `data` of an FCM message: the plaintext, or `None` when it does not
/// authenticate (wrong key, desktop or any altered byte).
pub(crate) fn open(key: &[u8; 32], desktop: &str, data: &serde_json::Value) -> Option<Vec<u8>> {
    let nonce: [u8; 12] = unb64(data["nonce"].as_str()?).try_into().ok()?;
    let mut buf = unb64(data["ciphertext"].as_str()?);
    let key = LessSafeKey::new(UnboundKey::new(&AES_256_GCM, key).ok()?);
    let aad = format!("ompdeck-android-push-v1:{desktop}");
    key.open_in_place(
        Nonce::assume_unique_for_key(nonce),
        Aad::from(aad.as_bytes()),
        &mut buf,
    )
    .ok()
    .map(|p| p.to_vec())
}

// ---- a loopback Google ---------------------------------------------------------------------------

#[derive(Debug)]
pub(crate) struct Request {
    pub path: String,
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).unwrap()
    }
}

pub(crate) struct Reply {
    status: u16,
    headers: Vec<(&'static str, String)>,
    body: String,
}

impl Reply {
    pub fn json(status: u16, body: serde_json::Value) -> Reply {
        Reply {
            status,
            headers: vec![("Content-Type", "application/json".to_owned())],
            body: body.to_string(),
        }
    }

    pub fn empty(status: u16) -> Reply {
        Reply {
            status,
            headers: Vec::new(),
            body: String::new(),
        }
    }

    pub fn with(mut self, name: &'static str, value: &str) -> Reply {
        self.headers.push((name, value.to_owned()));
        self
    }

    pub fn ok() -> Reply {
        Reply::json(200, serde_json::json!({"name": "projects/p/messages/1"}))
    }

    /// FCM's own error body (`error.status` and the `FcmError` detail).
    pub fn fcm_error(http: u16, status: &str, code: &str) -> Reply {
        Reply::json(
            http,
            serde_json::json!({"error": {
                "code": http,
                "message": "something about token ya29.secret",
                "status": status,
                "details": [{
                    "@type": "type.googleapis.com/google.firebase.fcm.v1.FcmError",
                    "errorCode": code,
                }],
            }}),
        )
    }

    pub fn token(n: usize, expires_in: u64) -> Reply {
        Reply::json(
            200,
            serde_json::json!({
                "access_token": format!("ya29.test-{n}"),
                "expires_in": expires_in,
                "token_type": "Bearer",
            }),
        )
    }
}

/// A loopback Google (token endpoint and FCM): `handler` answers each request, in order, and the
/// test reads what arrived.
pub(crate) struct Server {
    pub port: u16,
    got: mpsc::Receiver<Request>,
}

impl Server {
    pub fn start(mut handler: impl FnMut(&Request) -> Reply + Send + 'static) -> Server {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, got) = mpsc::channel();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 {
                    continue;
                }
                let path = line.split_whitespace().nth(1).unwrap_or("").to_owned();
                let mut headers = HashMap::new();
                loop {
                    let mut h = String::new();
                    reader.read_line(&mut h).unwrap();
                    let h = h.trim_end();
                    if h.is_empty() {
                        break;
                    }
                    let (k, v) = h.split_once(':').unwrap();
                    headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_owned());
                }
                let len: usize = headers
                    .get("content-length")
                    .map_or(0, |l| l.parse().unwrap());
                let mut body = vec![0; len];
                reader.read_exact(&mut body).unwrap();
                let request = Request {
                    path,
                    headers,
                    body,
                };
                let reply = handler(&request);
                let mut head = format!("HTTP/1.1 {} X\r\n", reply.status);
                for (k, v) in &reply.headers {
                    let _ = write!(head, "{k}: {v}\r\n");
                }
                write!(
                    stream,
                    "{head}Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                    reply.body.len(),
                    reply.body
                )
                .unwrap();
                if tx.send(request).is_err() {
                    return;
                }
            }
        });
        Server { port, got }
    }

    /// A Google whose token endpoint always works (`ya29.test-<n>`, an hour) and whose FCM
    /// endpoint answers with `send(n)` for the n-th message (from 0).
    pub fn google(mut send: impl FnMut(usize, &Request) -> Reply + Send + 'static) -> Server {
        let (mut tokens, mut sent) = (0, 0);
        Server::start(move |r| {
            if r.path == "/token" {
                tokens += 1;
                Reply::token(tokens, 3600)
            } else {
                sent += 1;
                send(sent - 1, r)
            }
        })
    }

    pub fn config(&self) -> Config {
        Config::with_endpoints(creds(), Endpoints::loopback(self.port, PROJECT))
    }

    pub fn next(&self) -> Request {
        self.got
            .recv_timeout(Duration::from_secs(10))
            .expect("a request to Google")
    }

    /// The next request that is a message (the token requests before it are skipped).
    pub fn next_message(&self) -> Request {
        loop {
            let r = self.next();
            if r.path != "/token" {
                return r;
            }
        }
    }

    pub fn quiet(&self, for_: Duration) {
        if let Ok(r) = self.got.recv_timeout(for_) {
            panic!("unexpected request: {} {:?}", r.path, r.headers);
        }
    }
}

pub(crate) const KEY: [u8; 32] = [7; 32];

pub(crate) fn job(device: &str, token: &str) -> Job {
    let expires = now_ms() + 60_000;
    Job {
        device: device.to_owned(),
        token: token.to_owned(),
        key: Zeroizing::new(KEY),
        desktop: DESKTOP.to_owned(),
        plaintext: plaintext(
            br#"{"web_push":8030,"notification":{"title":"OmpDeck"}}"#,
            expires,
        )
        .unwrap(),
        expires_unix_ms: expires,
    }
}

/// A started sender and what it reports gone.
pub(crate) fn sender(server: &Server) -> (Sender, mpsc::Receiver<Gone>) {
    let (tx, rx) = mpsc::channel();
    let report = move |g: Gone| {
        let _ = tx.send(g);
    };
    let s = Sender::start_with(
        Arc::new(server.config()),
        Arc::new(report),
        Duration::from_millis(20),
    )
    .unwrap();
    (s, rx)
}

// ---- credentials ---------------------------------------------------------------------------------

#[test]
fn a_service_account_is_validated_and_its_token_uri_ignored() {
    let c = creds();
    assert_eq!(c.project(), PROJECT);
    let config = Config::new(c);
    assert_eq!(
        config.endpoints.token,
        "https://oauth2.googleapis.com/token"
    );
    assert_eq!(
        config.endpoints.messages,
        "https://fcm.googleapis.com/v1/projects/ompdeck-test/messages:send"
    );
    assert!(config.endpoints.https_only);
    // The private key is never printed.
    let shown = format!("{:?}", config.credentials);
    assert!(!shown.contains("MII"), "{shown}");

    type AccountEdit = fn(&mut serde_json::Value);
    let bad: [(&str, AccountEdit); 9] = [
        ("type", |v| v["type"] = "authorized_user".into()),
        ("no type", |v| {
            v.as_object_mut().unwrap().remove("type");
        }),
        ("project uppercase", |v| {
            v["project_id"] = "OmpDeck-Test".into();
        }),
        ("project path", |v| {
            v["project_id"] = "ompdeck/../../x-test".into();
        }),
        ("project short", |v| v["project_id"] = "abc".into()),
        ("email", |v| v["client_email"] = "no at sign".into()),
        ("no key", |v| {
            v.as_object_mut().unwrap().remove("private_key");
        }),
        ("key text", |v| v["private_key"] = "not pem".into()),
        ("key bytes", |v| {
            v["private_key"] =
                "-----BEGIN PRIVATE KEY-----\nAAAA\n-----END PRIVATE KEY-----\n".into();
        }),
    ];
    for (what, edit) in bad {
        assert!(Credentials::parse(&account(edit)).is_err(), "{what}");
    }
    assert!(Credentials::parse(b"{").is_err());
    assert!(Credentials::parse(b"[]").is_err());
}

#[test]
fn credentials_are_read_from_the_secrets_dir_or_the_named_file() {
    let dir = tempfile::tempdir().unwrap();
    // Nothing configured: unsupported, quietly.
    assert!(load_from(dir.path(), None).is_none());
    // A file there is read at startup.
    std::fs::write(dir.path().join(CREDENTIALS_FILE), account(|_| {})).unwrap();
    let c = load_from(dir.path(), None).unwrap();
    assert_eq!(c.credentials.project(), PROJECT);
    // The named file wins, and a named file that is missing or broken is not replaced by
    // the one in the secrets dir.
    let other = dir.path().join("other.json");
    std::fs::write(
        &other,
        account(|v| v["project_id"] = "ompdeck-other".into()),
    )
    .unwrap();
    let c = load_from(dir.path(), Some(other.clone().into())).unwrap();
    assert_eq!(c.credentials.project(), "ompdeck-other");
    assert!(load_from(dir.path(), Some(dir.path().join("missing.json").into())).is_none());
    std::fs::write(&other, b"{}").unwrap();
    assert!(load_from(dir.path(), Some(other.into())).is_none());
    // A broken file in the secrets dir turns the feature off.
    std::fs::write(dir.path().join(CREDENTIALS_FILE), b"nope").unwrap();
    assert!(load_from(dir.path(), None).is_none());
    // So does one that is far too large.
    std::fs::write(
        dir.path().join(CREDENTIALS_FILE),
        vec![b' '; usize::try_from(MAX_ACCOUNT_FILE).unwrap() + 1],
    )
    .unwrap();
    assert!(load_from(dir.path(), None).is_none());
}

#[test]
fn the_oauth_assertion_is_an_rs256_jwt_for_the_fixed_scope_and_audience() {
    let c = creds();
    let jwt = c.assertion(TOKEN_URL, 1_700_000_000).unwrap();
    let parts: Vec<&str> = jwt.split('.').collect();
    assert_eq!(parts.len(), 3);
    let header: serde_json::Value = serde_json::from_slice(&unb64(parts[0])).unwrap();
    assert_eq!(header, serde_json::json!({"alg": "RS256", "typ": "JWT"}));
    let claims: serde_json::Value = serde_json::from_slice(&unb64(parts[1])).unwrap();
    assert_eq!(
        claims,
        serde_json::json!({
            "iss": "gateway@ompdeck-test.iam.gserviceaccount.com",
            "scope": "https://www.googleapis.com/auth/firebase.messaging",
            "aud": "https://oauth2.googleapis.com/token",
            "iat": 1_700_000_000u64,
            "exp": 1_700_003_600u64,
        })
    );
    UnparsedPublicKey::new(&RSA_PKCS1_2048_8192_SHA256, c.key.public().as_ref())
        .verify(
            format!("{}.{}", parts[0], parts[1]).as_bytes(),
            &unb64(parts[2]),
        )
        .unwrap();
}

// ---- the envelope ----------------------------------------------------------------------------------

#[test]
fn the_envelope_is_sealed_bound_to_the_desktop_and_carries_its_expiry() {
    let payload =
        br#"{"web_push":8030,"notification":{"title":"OmpDeck","body":"A session finished"}}"#;
    let plain = plaintext(payload, 1_900_000_000_000).unwrap();
    let value: serde_json::Value = serde_json::from_slice(&plain).unwrap();
    assert_eq!(value["expires_unix_ms"], 1_900_000_000_000u64);
    assert_eq!(value["web_push"], 8030);
    assert!(plaintext(b"[1]", 1).is_none());
    assert!(plaintext(b"nope", 1).is_none());

    let mut j = job("d-1", "tok");
    j.plaintext = plain.clone();
    let nonce = [9u8; 12];
    let body: serde_json::Value = serde_json::from_slice(&message(&j, nonce).unwrap()).unwrap();
    let m = &body["message"];
    assert_eq!(m["token"], "tok");
    assert_eq!(
        m["android"],
        serde_json::json!({"priority": "HIGH", "ttl": "3600s"})
    );
    let data = &m["data"];
    assert_eq!(data["v"], "1");
    assert_eq!(data["desktop"], DESKTOP);
    assert_eq!(unb64(data["nonce"].as_str().unwrap()), nonce);
    // Only the opaque envelope reaches Google: no title, text or route in the clear.
    let text = serde_json::to_string(m).unwrap();
    assert!(
        !text.contains("finished") && !text.contains("OmpDeck"),
        "{text}"
    );
    assert_eq!(data.as_object().unwrap().len(), 4);
    let ct = unb64(data["ciphertext"].as_str().unwrap());
    assert_eq!(ct.len(), plain.len() + 16, "ciphertext || 16-byte tag");
    assert_eq!(open(&KEY, DESKTOP, data).unwrap(), plain.to_vec());

    // The key, the desktop (AAD) and every byte of the ciphertext matter.
    assert!(open(&[8; 32], DESKTOP, data).is_none());
    assert!(open(&KEY, &"ff".repeat(32), data).is_none());
    let mut tampered = data.clone();
    let mut bytes = ct;
    bytes[0] ^= 1;
    tampered["ciphertext"] = b64(&bytes).into();
    assert!(open(&KEY, DESKTOP, &tampered).is_none());
}

// ---- the sender -------------------------------------------------------------------------------------

#[test]
fn a_message_is_posted_to_the_fixed_path_with_a_cached_access_token() {
    let server = Server::google(|_, _| Reply::ok());
    let (s, gone) = sender(&server);
    s.send(job("d-1", "token-one"));
    s.send(job("d-1", "token-one"));

    let t = server.next();
    assert_eq!(t.path, "/token");
    assert_eq!(
        t.headers["content-type"],
        "application/x-www-form-urlencoded"
    );
    let form = String::from_utf8(t.body.clone()).unwrap();
    let (grant, assertion) = form.split_once("&assertion=").unwrap();
    assert_eq!(
        grant,
        "grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Ajwt-bearer"
    );
    let signed = assertion.rsplit_once('.').unwrap();
    UnparsedPublicKey::new(&RSA_PKCS1_2048_8192_SHA256, creds().key.public().as_ref())
        .verify(signed.0.as_bytes(), &unb64(signed.1))
        .unwrap();

    let first = server.next();
    let second = server.next();
    assert_eq!(first.path, "/v1/projects/ompdeck-test/messages:send");
    assert_eq!(first.headers["authorization"], "Bearer ya29.test-1");
    assert_eq!(
        first.headers["content-type"],
        "application/json; charset=UTF-8"
    );
    // One token request served both messages.
    assert_eq!(second.headers["authorization"], "Bearer ya29.test-1");
    server.quiet(Duration::from_millis(300));

    // Each message decrypts for the phone, under a nonce of its own.
    let (a, b) = (first.json(), second.json());
    let (da, db) = (&a["message"]["data"], &b["message"]["data"]);
    assert_ne!(da["nonce"], db["nonce"]);
    assert_ne!(da["ciphertext"], db["ciphertext"]);
    for data in [da, db] {
        let plain: serde_json::Value =
            serde_json::from_slice(&open(&KEY, DESKTOP, data).unwrap()).unwrap();
        assert_eq!(plain["web_push"], 8030);
        assert!(plain["expires_unix_ms"].as_u64().unwrap() > now_ms());
    }
    assert!(gone.try_recv().is_err());
}

#[test]
fn an_access_token_close_to_expiry_is_renewed() {
    let mut tokens = 0;
    let server = Server::start(move |r| {
        if r.path == "/token" {
            tokens += 1;
            // Valid for less than the renewal margin: never reused.
            Reply::token(tokens, 60)
        } else {
            Reply::ok()
        }
    });
    let (s, _gone) = sender(&server);
    s.send(job("d-1", "t"));
    assert_eq!(server.next().path, "/token");
    assert_eq!(
        server.next_message().headers["authorization"],
        "Bearer ya29.test-1"
    );
    s.send(job("d-1", "t"));
    assert_eq!(server.next().path, "/token");
    assert_eq!(
        server.next_message().headers["authorization"],
        "Bearer ya29.test-2"
    );
}

#[test]
fn only_unregistered_clears_the_exact_registration() {
    // UNREGISTERED: reported once with the exact token and key, not retried.
    let server = Server::google(|_, _| Reply::fcm_error(404, "NOT_FOUND", "UNREGISTERED"));
    let (s, gone) = sender(&server);
    s.send(job("d-1", "dead-token"));
    server.next_message();
    let g = gone.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(
        g,
        Gone {
            device: "d-1".into(),
            token: "dead-token".into(),
            key: b64(&KEY),
        }
    );
    server.quiet(Duration::from_millis(300));
    drop(s);

    // Every other refusal leaves the registration alone and is not retried: a 400, a
    // sender-id mismatch (another project's token), missing permission, an unknown 404.
    for (http, status, code) in [
        (400, "INVALID_ARGUMENT", "INVALID_ARGUMENT"),
        (403, "PERMISSION_DENIED", "SENDER_ID_MISMATCH"),
        (403, "PERMISSION_DENIED", "PERMISSION_DENIED"),
        (404, "NOT_FOUND", "SOMETHING_ELSE"),
    ] {
        let server = Server::google(move |_, _| Reply::fcm_error(http, status, code));
        let (s, gone) = sender(&server);
        s.send(job("d-1", "live-token"));
        server.next_message();
        server.quiet(Duration::from_millis(300));
        assert!(gone.try_recv().is_err(), "{http} {code}");
        drop(s);
    }
    // An answer that is not even JSON is no UNREGISTERED either.
    let server = Server::google(|_, _| Reply::empty(404));
    let (s, gone) = sender(&server);
    s.send(job("d-1", "live-token"));
    server.next_message();
    server.quiet(Duration::from_millis(300));
    assert!(gone.try_recv().is_err());
    drop(s);
}

#[test]
fn a_rejected_service_account_is_not_a_dead_registration() {
    let server = Server::start(|r| {
        assert_eq!(r.path, "/token", "nothing is sent without an access token");
        Reply::json(
            400,
            serde_json::json!({"error": "invalid_grant", "error_description": "Invalid JWT Signature."}),
        )
    });
    let (s, gone) = sender(&server);
    s.send(job("d-1", "live-token"));
    assert_eq!(server.next().path, "/token");
    // Not retried within the attempt, and nothing is cleared.
    server.quiet(Duration::from_millis(300));
    assert!(gone.try_recv().is_err());
    // The next notification asks for a token again (the account may have been fixed).
    s.send(job("d-1", "live-token"));
    assert_eq!(server.next().path, "/token");
    assert!(gone.try_recv().is_err());
}

#[test]
fn transient_errors_are_retried_at_most_three_times() {
    let server = Server::google(|n, _| match n {
        0 => Reply::fcm_error(503, "UNAVAILABLE", "UNAVAILABLE"),
        1 => Reply::fcm_error(429, "RESOURCE_EXHAUSTED", "QUOTA_EXCEEDED").with("Retry-After", "0"),
        _ => Reply::ok(),
    });
    let (s, gone) = sender(&server);
    s.send(job("d-1", "t"));
    for _ in 0..3 {
        server.next_message();
    }
    server.quiet(Duration::from_millis(400));
    assert!(gone.try_recv().is_err());
    drop(s);

    let server = Server::google(|_, _| Reply::fcm_error(500, "INTERNAL", "INTERNAL"));
    let (s, gone) = sender(&server);
    s.send(job("d-1", "t"));
    for _ in 0..3 {
        server.next_message();
    }
    server.quiet(Duration::from_millis(800));
    assert!(gone.try_recv().is_err());
    drop(s);
}

#[test]
fn a_refused_access_token_is_renewed_and_the_push_retried() {
    let server = Server::google(|n, _| {
        if n == 0 {
            Reply::fcm_error(401, "UNAUTHENTICATED", "UNAUTHENTICATED")
        } else {
            Reply::ok()
        }
    });
    let (s, _gone) = sender(&server);
    s.send(job("d-1", "t"));
    assert_eq!(server.next().path, "/token");
    assert_eq!(
        server.next_message().headers["authorization"],
        "Bearer ya29.test-1"
    );
    assert_eq!(server.next().path, "/token");
    assert_eq!(
        server.next_message().headers["authorization"],
        "Bearer ya29.test-2"
    );
    server.quiet(Duration::from_millis(300));
}

#[test]
fn a_redirect_is_not_followed() {
    let server = Server::google(|_, _| Reply::empty(307).with("Location", "http://127.0.0.1:9/x"));
    let (s, gone) = sender(&server);
    s.send(job("d-1", "t"));
    server.next_message();
    server.quiet(Duration::from_millis(300));
    assert!(gone.try_recv().is_err());
}

#[test]
fn an_expired_push_is_not_sent_and_a_purge_makes_a_retry_final() {
    let server = Server::google(|_, _| Reply::ok());
    let (s, _gone) = sender(&server);
    let mut late = job("d-1", "t");
    late.expires_unix_ms = now_ms() - 1;
    s.send(late);
    server.quiet(Duration::from_millis(300));
    drop(s);

    // The retry is due in five seconds; the purge comes first and the push never repeats.
    let server = Server::google(|_, _| {
        Reply::fcm_error(429, "RESOURCE_EXHAUSTED", "QUOTA_EXCEEDED").with("Retry-After", "5")
    });
    let (s, _gone) = sender(&server);
    s.send(job("d-1", "t"));
    server.next_message();
    s.purge("d-1");
    server.quiet(Duration::from_secs(6));
}
