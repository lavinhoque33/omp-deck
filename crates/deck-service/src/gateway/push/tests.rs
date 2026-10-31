use std::collections::HashMap;
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::TcpListener;
use std::path::Path;
use std::sync::mpsc;
use std::time::Instant;

use deck_core::config::NotifyConfig;
use web_push_native::p256::elliptic_curve::sec1::ToEncodedPoint as _;

use super::*;
use crate::controller::{ServiceEpoch, SessionKey, SessionTarget};
use crate::gateway::protocol::Permissions;
use crate::notifications::{Effect, NotificationCenter, Observation};
use crate::status::TabStatus;

const STRICT: Policy = Policy {
    allow_loopback: false,
};
const LOOPBACK: Policy = Policy {
    allow_loopback: true,
};
const ORIGIN: &str = "https://relay.example";

fn b64(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

fn unb64(text: &str) -> Vec<u8> {
    URL_SAFE_NO_PAD.decode(text).unwrap()
}

/// A browser's subscription keys: the user agent's secret and `auth`.
struct Ua {
    secret: p256::SecretKey,
    auth: [u8; 16],
}

impl Ua {
    fn new() -> Ua {
        loop {
            let mut bytes = [0u8; 32];
            getrandom::fill(&mut bytes).unwrap();
            if let Ok(secret) = p256::SecretKey::from_slice(&bytes) {
                let mut auth = [0u8; 16];
                getrandom::fill(&mut auth).unwrap();
                return Ua { secret, auth };
            }
        }
    }

    fn p256dh(&self) -> String {
        b64(self.secret.public_key().to_encoded_point(false).as_bytes())
    }

    fn decrypt(&self, body: Vec<u8>) -> Vec<u8> {
        web_push_native::decrypt(body, &self.secret, &Auth::clone_from_slice(&self.auth)).unwrap()
    }
}

/// One request the receiver got.
#[derive(Debug)]
struct Received {
    path: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

/// A loopback push service: answers each request with the next of `statuses` (the last one
/// repeats) and reports what it received.
struct Receiver {
    port: u16,
    got: mpsc::Receiver<Received>,
}

impl Receiver {
    fn start(statuses: &[u16]) -> Receiver {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, got) = mpsc::channel();
        let statuses = statuses.to_vec();
        std::thread::spawn(move || {
            for (n, stream) in listener.incoming().enumerate() {
                let Ok(mut stream) = stream else { return };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
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
                let status = statuses[n.min(statuses.len() - 1)];
                let extra = if status == 429 {
                    "Retry-After: 0\r\n"
                } else {
                    ""
                };
                write!(
                    stream,
                    "HTTP/1.1 {status} X\r\n{extra}Content-Length: 0\r\nConnection: close\r\n\r\n"
                )
                .unwrap();
                if tx
                    .send(Received {
                        path,
                        headers,
                        body,
                    })
                    .is_err()
                {
                    return;
                }
            }
        });
        Receiver { port, got }
    }

    fn endpoint(&self, name: &str) -> String {
        format!("http://127.0.0.1:{}/push/{name}", self.port)
    }

    fn next(&self) -> Received {
        self.got
            .recv_timeout(Duration::from_secs(10))
            .expect("a push request")
    }

    fn quiet(&self, for_: Duration) {
        if let Ok(r) = self.got.recv_timeout(for_) {
            panic!("unexpected push request: {r:?}");
        }
    }
}

fn device(id: &str, key: u8) -> Device {
    Device {
        id: id.to_owned(),
        name: id.to_owned(),
        platform: "test".to_owned(),
        public: [key; 32],
        token_hash: [key; 32],
        permissions: Permissions::default(),
        last_ts: 1,
        created_unix_ms: 1,
        last_seen_unix_ms: 1,
        push: None,
    }
}

fn registry(dir: &Path, ids: &[&str]) -> Registry {
    let mut reg = Registry::load(&dir.join("devices.json")).unwrap();
    for (i, id) in ids.iter().enumerate() {
        reg.add(device(id, u8::try_from(i + 1).unwrap())).unwrap();
    }
    reg
}

fn subscribe_ua(reg: &mut Registry, id: &str, endpoint: &str, ua: &Ua) {
    let sub = check_with(endpoint, &ua.p256dh(), &b64(&ua.auth), LOOPBACK).unwrap();
    subscribe(reg, id, sub).unwrap();
}

fn new_key() -> VapidKey {
    VapidKey::from_pkcs8(&VapidKey::generate_pkcs8().unwrap()).unwrap()
}

fn push() -> Push {
    Push::with_key(Some(new_key()), LOOPBACK, None, Arc::new(|| {}))
}

fn offer<'a>(kind: MobileKind, session: Option<&'a str>) -> Offer<'a> {
    Offer {
        kind,
        title: "fix the parser",
        session,
        fallback: "tmux:x",
        origin: ORIGIN,
    }
}

fn json(bytes: &[u8]) -> serde_json::Value {
    serde_json::from_slice(bytes).unwrap()
}

#[test]
fn rfc8291_appendix_a_decrypts() {
    let ct = unb64(
        "DGv6ra1nlYgDCS1FRnbzlwAAEABBBP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27mlmlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A_yl95bQpu6cVPTpK4Mqgkf1CXztLVBSt2Ks3oZwbuwXPXLWyouBWLVWGNWQexSgSxsj_Qulcy4a-fN",
    );
    let secret =
        p256::SecretKey::from_slice(&unb64("q1dXpw3UpT5VOmu_cf_v6ih07Aems3njxI-JWgLcM94")).unwrap();
    let auth = Auth::clone_from_slice(&unb64("BTBZMqHH6r4Tts7J_aSIgg"));
    assert_eq!(
        web_push_native::decrypt(ct, &secret, &auth).unwrap(),
        b"When I grow up, I want to be a watermelon"
    );
}

#[test]
fn subscriptions_are_parsed_by_hand_and_bad_ones_refused_without_panicking() {
    let ua = Ua::new();
    let (key, auth) = (ua.p256dh(), b64(&ua.auth));
    let ok = |e: &str, k: &str, a: &str| parse(e, k, a, STRICT).map(|_| ());
    assert!(ok("https://push.example.com/abc", &key, &auth).is_ok());
    // Padded base64url is accepted too.
    assert!(ok("https://push.example.com/abc", &key, &format!("{auth}==")).is_ok());
    let refused = [
        ok("http://push.example.com/abc", &key, &auth),
        ok("ftp://push.example.com/abc", &key, &auth),
        ok("not a url", &key, &auth),
        ok("/relative", &key, &auth),
        ok("https://user@push.example.com/abc", &key, &auth),
        ok(
            &format!("https://push.example.com/{}", "a".repeat(3000)),
            &key,
            &auth,
        ),
        ok("https://push.example.com/abc", "AAAA", &auth),
        ok("https://push.example.com/abc", "!!!", &auth),
        // A 15- and a 17-byte auth: the crate's serde would panic on these.
        ok("https://push.example.com/abc", &key, &b64(&[1; 15])),
        ok("https://push.example.com/abc", &key, &b64(&[1; 17])),
    ];
    for (i, r) in refused.into_iter().enumerate() {
        assert!(matches!(r, Err(PushError::Invalid(_))), "case {i}: {r:?}");
    }
    // http:// only under the test override.
    assert!(parse("http://127.0.0.1:1/x", &key, &auth, LOOPBACK).is_ok());
    // Only port 443 (explicit or implied) outside the test override.
    assert!(ok("https://push.example.com:443/abc", &key, &auth).is_ok());
    for port in [80, 8443, 22, 6379] {
        let r = ok(&format!("https://push.example.com:{port}/abc"), &key, &auth);
        assert!(
            matches!(r, Err(PushError::Invalid(_))),
            "port {port}: {r:?}"
        );
    }
    assert!(parse("https://127.0.0.1:8443/x", &key, &auth, LOOPBACK).is_ok());
}

#[test]
fn navigate_is_exactly_the_pwa_origin_and_session_hash() {
    let tag = topic("s");
    for origin in ["https://relay.example", "https://relay.example/"] {
        let p = json(&payload(MobileKind::Ended, None, origin, Some("sess-1"), &tag).unwrap());
        assert_eq!(
            p["notification"]["navigate"],
            "https://relay.example/#s=sess-1"
        );
        let p = json(&payload(MobileKind::Ended, None, origin, None, &tag).unwrap());
        assert_eq!(p["notification"]["navigate"], "https://relay.example/");
    }
}

#[test]
fn private_endpoints_are_refused_at_subscribe_time_without_the_test_override() {
    let ua = Ua::new();
    for endpoint in [
        "https://127.0.0.1/push",
        "https://10.1.2.3/push",
        "https://[::1]/push",
        "https://[::ffff:127.0.0.1]/push",
        "https://100.64.0.9/push",
        "https://169.254.169.254/latest",
    ] {
        let r = check_with(endpoint, &ua.p256dh(), &b64(&ua.auth), STRICT);
        assert!(matches!(r, Err(PushError::Blocked)), "{endpoint}: {r:?}");
    }
    // The override admits loopback, nothing more.
    let r = check_with(
        "https://10.1.2.3/push",
        &ua.p256dh(),
        &b64(&ua.auth),
        LOOPBACK,
    );
    assert!(matches!(r, Err(PushError::Blocked)), "{r:?}");
    assert!(
        check_with(
            "http://127.0.0.1:9/p",
            &ua.p256dh(),
            &b64(&ua.auth),
            LOOPBACK
        )
        .is_ok()
    );
}

#[test]
fn a_private_endpoint_is_skipped_at_send_time() {
    // Stored under the override, then offered by a sender without it: nothing is sent (the
    // send-time resolver itself is covered in `ssrf`).
    let rx = Receiver::start(&[201]);
    let ua = Ua::new();
    let dir = tempfile::tempdir().unwrap();
    let mut reg = registry(dir.path(), &["d-1"]);
    subscribe_ua(&mut reg, "d-1", &rx.endpoint("a"), &ua);
    let mut strict = Push::with_key(Some(new_key()), STRICT, None, Arc::new(|| {}));
    strict.offer(
        reg.devices(),
        |_| false,
        &offer(MobileKind::TurnFinished, Some("s")),
    );
    rx.quiet(Duration::from_millis(500));
}

#[test]
fn the_gateway_publishes_its_stored_vapid_key() {
    let dir = tempfile::tempdir().unwrap();
    let secrets = dir.path().join("secrets");
    let opened = Push::open(&secrets, Arc::new(|| {}));
    assert_eq!(opened.public_key().len(), 87);
    let again = Push::open(&secrets, Arc::new(|| {}));
    assert_eq!(opened.public_key(), again.public_key());
    assert_eq!(
        vapid::load_or_create(&secrets.join(KEY_FILE))
            .unwrap()
            .public_key(),
        opened.public_key()
    );
    // A broken key file turns push off; it is kept, never replaced.
    std::fs::write(secrets.join(KEY_FILE), b"short").unwrap();
    assert_eq!(Push::open(&secrets, Arc::new(|| {})).public_key(), "");
    assert_eq!(std::fs::read(secrets.join(KEY_FILE)).unwrap(), b"short");
}

#[test]
fn subscription_storage_keeps_prefs_and_is_deleted_with_the_device() {
    let dir = tempfile::tempdir().unwrap();
    let mut reg = registry(dir.path(), &["d-1"]);
    let ua = Ua::new();
    let prefs = PushPrefs {
        attention: false,
        show_titles: true,
        ..PushPrefs::default()
    };
    assert!(matches!(
        set_prefs(&mut reg, "d-1", prefs),
        Err(PushError::NotSubscribed)
    ));
    assert!(matches!(
        unsubscribe(&mut reg, "nope"),
        Err(PushError::NoDevice)
    ));
    assert!(!state_view(reg.get("d-1").unwrap()).subscribed);
    subscribe_ua(&mut reg, "d-1", "http://127.0.0.1:9/a", &ua);
    set_prefs(&mut reg, "d-1", prefs).unwrap();
    let since = reg.get("d-1").unwrap().push.as_ref().unwrap().since_unix_ms;
    assert!(since > 0);
    // A browser refresh resubscribes: same endpoint keeps `since`, prefs stay either way.
    subscribe_ua(&mut reg, "d-1", "http://127.0.0.1:9/a", &ua);
    subscribe_ua(&mut reg, "d-1", "http://127.0.0.1:9/b", &ua);
    let view = state_view(reg.get("d-1").unwrap());
    assert!(view.subscribed);
    assert_eq!(view.prefs, prefs);
    let reloaded = Registry::load(&dir.path().join("devices.json")).unwrap();
    assert_eq!(
        reloaded.get("d-1").unwrap().push.as_ref().unwrap().endpoint,
        "http://127.0.0.1:9/b"
    );
    unsubscribe(&mut reg, "d-1").unwrap();
    assert_eq!(reg.get("d-1").unwrap().push, None);
    // Revoke (and unpair) remove the record, subscription included.
    subscribe_ua(&mut reg, "d-1", "http://127.0.0.1:9/a", &ua);
    reg.remove("d-1").unwrap();
    let text = std::fs::read_to_string(dir.path().join("devices.json")).unwrap();
    assert!(!text.contains("127.0.0.1:9"), "{text}");
}

#[test]
fn payloads_are_declarative_generic_by_default_and_bounded() {
    let tag = topic("sess-1");
    assert_eq!(tag.len(), 22);
    assert_eq!(tag, topic("sess-1"));
    assert_ne!(tag, topic("sess-2"));
    let p = json(&payload(MobileKind::Attention, None, ORIGIN, Some("sess-1"), &tag).unwrap());
    assert_eq!(
        p,
        serde_json::json!({
            "web_push": 8030,
            "notification": {
                "title": "OmpDeck",
                "body": "A session needs your attention",
                "navigate": "https://relay.example/#s=sess-1",
                "tag": tag,
                "silent": false,
            }
        })
    );
    let long = "é".repeat(200);
    let p = payload(MobileKind::Ended, Some(&long), ORIGIN, None, &tag).unwrap();
    assert!(p.len() <= MAX_PAYLOAD);
    let p = json(&p);
    assert_eq!(p["notification"]["navigate"], "https://relay.example/");
    assert_eq!(
        p["notification"]["body"],
        format!("{} ended", "é".repeat(TITLE_CHARS))
    );
    // A title that cannot fit falls back to the generic text.
    let huge_origin = format!("https://{}", "a".repeat(800));
    let p = json(
        &payload(
            MobileKind::TurnFinished,
            Some(&long),
            &huge_origin,
            None,
            &tag,
        )
        .unwrap(),
    );
    assert_eq!(p["notification"]["body"], "A session finished");
}

/// A session the center watches turns busy, then idle: one canonical "turn finished".
fn one_finished_turn(c: &mut NotificationCenter, key: &SessionTarget) -> Vec<Effect> {
    let cfg = NotifyConfig::default();
    let obs = |status| Observation {
        target: key,
        status,
        exit_code: None,
        session: None,
        title: "fix the parser",
        project: "proj",
        visible: false,
    };
    let t0 = Instant::now();
    c.tick(t0, &[obs(TabStatus::Busy)], false, &cfg);
    let mut effects = Vec::new();
    for i in 0..200 {
        effects.extend(
            c.tick(
                t0 + Duration::from_secs(10 + i),
                &[obs(TabStatus::Idle)],
                false,
                &cfg,
            )
            .effects,
        );
        if !c.inbox().entries().is_empty() && i > 3 {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    effects
}

/// What `Engine::apply_effects` does with the center's effects (prefs and presence decide).
fn apply(push: &mut Push, reg: &Registry, present: &[&str], effects: Vec<Effect>, session: &str) {
    for e in effects {
        if let Effect::Mobile {
            target,
            kind,
            title,
        } = e
        {
            let fallback = target.key.to_string();
            push.offer(
                reg.devices(),
                |id| present.contains(&id),
                &Offer {
                    kind,
                    title: &title,
                    session: Some(session),
                    fallback: &fallback,
                    origin: ORIGIN,
                },
            );
        }
    }
}

#[test]
fn a_canonical_event_reaches_a_loopback_push_service_once_and_decrypts() {
    let rx = Receiver::start(&[201]);
    let ua = Ua::new();
    let mut push = push();
    let dir = tempfile::tempdir().unwrap();
    let mut reg = registry(dir.path(), &["d-1"]);
    subscribe_ua(&mut reg, "d-1", &rx.endpoint("one"), &ua);
    let key = SessionTarget {
        epoch: ServiceEpoch::current(),
        key: SessionKey::tmux("omp-deck", "p1"),
        generation: 1,
    };
    let mut center = NotificationCenter::new(Arc::new(|| {}));
    let effects = one_finished_turn(&mut center, &key);
    apply(&mut push, &reg, &[], effects, "sess-1");

    let got = rx.next();
    assert_eq!(got.path, "/push/one");
    assert_eq!(got.headers["ttl"], "3600");
    assert_eq!(got.headers["urgency"], "normal");
    assert_eq!(got.headers["topic"], topic("sess-1"));
    assert_eq!(got.headers["content-encoding"], "aes128gcm");
    let authorization = &got.headers["authorization"];
    assert!(authorization.starts_with("vapid t="), "{authorization}");
    assert!(
        authorization.ends_with(&format!(", k={}", push.public_key())),
        "{authorization}"
    );
    let jwt = authorization["vapid t=".len()..]
        .split(", k=")
        .next()
        .unwrap();
    let claims = json(&unb64(jwt.split('.').nth(1).unwrap()));
    assert_eq!(claims["aud"], format!("http://127.0.0.1:{}", rx.port));
    assert_eq!(claims["sub"], ORIGIN);
    let plain = ua.decrypt(got.body);
    assert_eq!(
        json(&plain),
        serde_json::json!({
            "web_push": 8030,
            "notification": {
                "title": "OmpDeck",
                "body": "A session finished",
                "navigate": "https://relay.example/#s=sess-1",
                "tag": topic("sess-1"),
                "silent": false,
            }
        })
    );
    // One canonical event, one push.
    rx.quiet(Duration::from_millis(500));
}

#[test]
fn prefs_and_presence_decide_per_device() {
    let rx = Receiver::start(&[201]);
    let dir = tempfile::tempdir().unwrap();
    let mut reg = registry(
        dir.path(),
        &["d-want", "d-off", "d-here", "d-titles", "d-none"],
    );
    let uas: Vec<Ua> = (0..4).map(|_| Ua::new()).collect();
    for (ua, id) in uas.iter().zip(["d-want", "d-off", "d-here", "d-titles"]) {
        subscribe_ua(&mut reg, id, &rx.endpoint(id), ua);
    }
    set_prefs(
        &mut reg,
        "d-off",
        PushPrefs {
            attention: false,
            ..PushPrefs::default()
        },
    )
    .unwrap();
    set_prefs(
        &mut reg,
        "d-titles",
        PushPrefs {
            show_titles: true,
            ..PushPrefs::default()
        },
    )
    .unwrap();
    let mut push = push();
    push.offer(
        reg.devices(),
        |id| id == "d-here",
        &offer(MobileKind::Attention, Some("sess-9")),
    );
    let mut got: Vec<Received> = vec![rx.next(), rx.next()];
    rx.quiet(Duration::from_millis(500));
    got.sort_by(|a, b| a.path.cmp(&b.path));
    assert_eq!(got[0].path, "/push/d-titles");
    assert_eq!(got[1].path, "/push/d-want");
    for r in &got {
        assert_eq!(r.headers["urgency"], "high");
    }
    let titled = json(&uas[3].decrypt(got.remove(0).body));
    assert_eq!(
        titled["notification"]["body"],
        "fix the parser needs your attention"
    );
    let generic = json(&uas[0].decrypt(got.remove(0).body));
    assert_eq!(
        generic["notification"]["body"],
        "A session needs your attention"
    );
    // A kind the device turned off sends nothing; the others still go.
    push.offer(
        reg.devices(),
        |_| false,
        &offer(MobileKind::Ended, Some("sess-9")),
    );
    let mut paths: Vec<String> = (0..4).map(|_| rx.next().path).collect();
    paths.sort();
    assert_eq!(
        paths,
        [
            "/push/d-here",
            "/push/d-off",
            "/push/d-titles",
            "/push/d-want"
        ]
    );
}

#[test]
fn a_gone_subscription_is_deleted() {
    let rx = Receiver::start(&[410]);
    let ua = Ua::new();
    let dir = tempfile::tempdir().unwrap();
    let mut reg = registry(dir.path(), &["d-1"]);
    let endpoint = rx.endpoint("gone");
    subscribe_ua(&mut reg, "d-1", &endpoint, &ua);
    let mut push = push();
    push.offer(
        reg.devices(),
        |_| false,
        &offer(MobileKind::TurnFinished, Some("s")),
    );
    rx.next();
    let gone = (0..250)
        .find_map(|_| {
            let g = push.take_gone();
            if g.is_empty() {
                std::thread::sleep(Duration::from_millis(20));
                None
            } else {
                Some(g)
            }
        })
        .expect("the 410 is reported");
    assert_eq!(
        gone,
        [Gone {
            device: "d-1".into(),
            endpoint: endpoint.clone(),
        }]
    );
    // Not retried.
    rx.quiet(Duration::from_millis(300));
    // A device that resubscribed meanwhile keeps its new subscription.
    subscribe_ua(&mut reg, "d-1", "http://127.0.0.1:9/new", &ua);
    assert!(!forget(&mut reg, &gone[0]).unwrap());
    assert!(reg.get("d-1").unwrap().push.is_some());
    subscribe_ua(&mut reg, "d-1", &endpoint, &ua);
    assert!(forget(&mut reg, &gone[0]).unwrap());
    assert_eq!(reg.get("d-1").unwrap().push, None);
    let reloaded = Registry::load(&dir.path().join("devices.json")).unwrap();
    assert_eq!(reloaded.get("d-1").unwrap().push, None);
}

#[test]
fn server_errors_and_rate_limits_are_retried_at_most_three_times() {
    let rx = Receiver::start(&[503, 429, 201]);
    let ua = Ua::new();
    let dir = tempfile::tempdir().unwrap();
    let mut reg = registry(dir.path(), &["d-1"]);
    subscribe_ua(&mut reg, "d-1", &rx.endpoint("r"), &ua);
    let sub = reg.get("d-1").unwrap().push.clone().unwrap();
    let job = sender::Job {
        device: "d-1".into(),
        endpoint: sub.endpoint,
        p256dh: sub.p256dh,
        auth: sub.auth,
        payload: b"{}".to_vec(),
        urgency: "normal",
        topic: topic("s"),
        contact: ORIGIN.into(),
    };
    let s = sender::Sender::start_with(
        Arc::new(new_key()),
        LOOPBACK,
        Arc::new(|_: Gone| {}),
        Duration::from_millis(50),
    )
    .unwrap();
    s.send(job.clone());
    let tries: Vec<Received> = (0..3).map(|_| rx.next()).collect();
    // The VAPID token is reused for the same push service.
    assert!(
        tries
            .iter()
            .all(|r| r.headers["authorization"] == tries[0].headers["authorization"])
    );
    assert_eq!(ua.decrypt(tries[2].body.clone()), b"{}");
    rx.quiet(Duration::from_millis(400));
    drop(s);

    // Three server errors: dropped after the third.
    let rx = Receiver::start(&[500]);
    let s = sender::Sender::start_with(
        Arc::new(new_key()),
        LOOPBACK,
        Arc::new(|_: Gone| {}),
        Duration::from_millis(20),
    )
    .unwrap();
    s.send(sender::Job {
        endpoint: rx.endpoint("e"),
        ..job
    });
    for _ in 0..3 {
        rx.next();
    }
    rx.quiet(Duration::from_millis(800));
}
