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
/// A desktop id: the lowercase hex of a gateway key.
const DESKTOP: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";

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
        android_push: None,
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

fn offer<'a>(kind: MobileKind, opens: &'a Opens) -> Offer<'a> {
    Offer {
        kind,
        title: "fix the parser",
        opens: Some(opens),
        fallback: "tmux:x",
        origin: ORIGIN,
        desktop: DESKTOP,
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
fn navigate_is_exactly_the_pwa_origin_and_the_session_or_live_link() {
    let tag = topic("s");
    let session = Opens::Session("sess-1".to_owned());
    // A session the gateway has no id for yet (just started) opens by live key, never by an id
    // the app would look up as an omp session.
    let live = Opens::Live("tmux:24:omp-deck:omp-new-17&x#y".to_owned());
    for origin in ["https://relay.example", "https://relay.example/"] {
        let p = json(&payload(MobileKind::Ended, None, origin, Some(&session), &tag).unwrap());
        assert_eq!(
            p["notification"]["navigate"],
            "https://relay.example/#s=sess-1"
        );
        let p = json(&payload(MobileKind::TurnFinished, None, origin, Some(&live), &tag).unwrap());
        assert_eq!(
            p["notification"]["navigate"],
            "https://relay.example/#route=conversation&live=tmux%3A24%3Aomp-deck%3Aomp-new-17%26x%23y"
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
        &offer(MobileKind::TurnFinished, &Opens::Session("s".to_owned())),
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
    assert!(!state_view(reg.get("d-1").unwrap(), None).subscribed);
    subscribe_ua(&mut reg, "d-1", "http://127.0.0.1:9/a", &ua);
    set_prefs(&mut reg, "d-1", prefs).unwrap();
    let since = reg.get("d-1").unwrap().push.as_ref().unwrap().since_unix_ms;
    assert!(since > 0);
    // A browser refresh resubscribes: same endpoint keeps `since`, prefs stay either way.
    subscribe_ua(&mut reg, "d-1", "http://127.0.0.1:9/a", &ua);
    subscribe_ua(&mut reg, "d-1", "http://127.0.0.1:9/b", &ua);
    let view = state_view(reg.get("d-1").unwrap(), None);
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
    let sess = Opens::Session("sess-1".to_owned());
    let p = json(&payload(MobileKind::Attention, None, ORIGIN, Some(&sess), &tag).unwrap());
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
        omp_title: true,
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
                    opens: Some(&Opens::Session(session.to_owned())),
                    fallback: &fallback,
                    origin: ORIGIN,
                    desktop: DESKTOP,
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
        &offer(MobileKind::Attention, &Opens::Session("sess-9".to_owned())),
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
        &offer(MobileKind::Ended, &Opens::Session("sess-9".to_owned())),
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
        &offer(MobileKind::TurnFinished, &Opens::Session("s".to_owned())),
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

// ---- native Android registrations ----------------------------------------------------------------

use super::fcm::tests as google;

/// device `id` registers `token` natively (under the shared test key); whether that changed it.
fn android(reg: &mut Registry, id: &str, token: &str) -> bool {
    let sub = check_android(token, &b64(&google::KEY), DESKTOP, DESKTOP, true).unwrap();
    subscribe_android(reg, id, sub).unwrap()
}

fn android_push(server: &google::Server) -> Push {
    push().with_fcm(Some(server.config()))
}

#[test]
fn android_registrations_are_validated_without_the_network() {
    let key = b64(&google::KEY);
    let check = |token: &str, key: &str, desktop: &str, configured| {
        check_android(token, key, desktop, DESKTOP, configured)
    };
    assert!(check("tok", &key, DESKTOP, true).is_ok());
    // No FCM account: the gateway says so, whatever else is true.
    assert!(matches!(
        check("tok", &key, DESKTOP, false),
        Err(PushError::AndroidUnsupported)
    ));
    // Another desktop's registration, and a desktop id that is not 64 lowercase hex digits.
    assert!(matches!(
        check("tok", &key, &"ff".repeat(32), true),
        Err(PushError::WrongDesktop)
    ));
    for bad in [
        DESKTOP.to_uppercase(),
        DESKTOP[..62].to_owned(),
        String::new(),
        format!("{}g", &DESKTOP[..63]),
    ] {
        assert!(matches!(
            check("tok", &key, &bad, true),
            Err(PushError::Invalid(_))
        ));
    }
    // A key is exactly 32 bytes of base64url.
    for bad in [
        String::new(),
        "AAAA".to_owned(),
        b64(&[1; 31]),
        b64(&[1; 33]),
        "!!!".to_owned(),
        "A".repeat(500),
    ] {
        assert!(matches!(
            check("tok", &bad, DESKTOP, true),
            Err(PushError::Invalid(_))
        ));
    }
    // A token is bounded visible ASCII.
    for bad in [
        String::new(),
        "a b".to_owned(),
        "a\"b".to_owned(),
        "x".repeat(5000),
        "tök".to_owned(),
    ] {
        assert!(matches!(
            check(&bad, &key, DESKTOP, true),
            Err(PushError::Invalid(_))
        ));
    }
    // Stored in the canonical form, never printed.
    let a = check("tok", &format!("{key}="), DESKTOP, true).unwrap();
    assert_eq!(a.key, key);
    let shown = format!("{a:?}");
    assert!(!shown.contains("tok") && !shown.contains(&key), "{shown}");
}

#[test]
fn an_android_registration_keeps_prefs_across_refresh_and_transport_and_goes_with_the_device() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("devices.json");
    let mut reg = registry(dir.path(), &["d-1"]);
    let prefs = PushPrefs {
        attention: false,
        show_titles: true,
        ..PushPrefs::default()
    };
    // A device without a push subscription is stored exactly as before this feature.
    assert!(
        !std::fs::read_to_string(&path)
            .unwrap()
            .contains("android_push")
    );
    assert!(matches!(
        set_prefs(&mut reg, "d-1", prefs),
        Err(PushError::NotSubscribed)
    ));
    assert!(matches!(
        subscribe_android(
            &mut reg,
            "nope",
            check_android("t", &b64(&google::KEY), DESKTOP, DESKTOP, true).unwrap()
        ),
        Err(PushError::NoDevice)
    ));

    assert!(
        android(&mut reg, "d-1", "token-1"),
        "a first registration is a change"
    );
    let d = reg.get("d-1").unwrap();
    assert!(d.push.is_none() && d.has_push() && d.view(false).push);
    assert!(state_view(d, None).subscribed);
    set_prefs(&mut reg, "d-1", prefs).unwrap();
    let since = state_view(reg.get("d-1").unwrap(), None)
        .since_unix_ms
        .unwrap();
    assert!(since > 0);
    // The same registration again changes nothing (nothing to purge); a refreshed token keeps
    // the prefs; so does a new key.
    assert!(!android(&mut reg, "d-1", "token-1"));
    assert_eq!(state_view(reg.get("d-1").unwrap(), None).prefs, prefs);
    assert_eq!(
        state_view(reg.get("d-1").unwrap(), None).since_unix_ms,
        Some(since)
    );
    assert!(android(&mut reg, "d-1", "token-2"));
    assert_eq!(state_view(reg.get("d-1").unwrap(), None).prefs, prefs);
    let rekeyed = check_android("token-2", &b64(&[9; 32]), DESKTOP, DESKTOP, true).unwrap();
    assert!(subscribe_android(&mut reg, "d-1", rekeyed).unwrap());
    assert_eq!(state_view(reg.get("d-1").unwrap(), None).prefs, prefs);

    // It survives a restart, key included, in the one registry file.
    let reloaded = Registry::load(&path).unwrap();
    let a = reloaded.get("d-1").unwrap().android_push.clone().unwrap();
    assert_eq!(
        (a.token.as_str(), a.key, a.prefs),
        ("token-2", b64(&[9; 32]), prefs)
    );
    let device_debug = format!("{:?}", reloaded.get("d-1").unwrap());
    assert!(!device_debug.contains("token-2"), "{device_debug}");

    // The transports exclude each other, and the prefs follow the device across them.
    let ua = Ua::new();
    let sub = check_with(
        "http://127.0.0.1:9/a",
        &ua.p256dh(),
        &b64(&ua.auth),
        LOOPBACK,
    )
    .unwrap();
    assert!(
        subscribe(&mut reg, "d-1", sub).unwrap(),
        "it replaced an Android registration"
    );
    let d = reg.get("d-1").unwrap();
    assert!(d.android_push.is_none() && d.push.is_some());
    assert_eq!(state_view(d, None).prefs, prefs);
    assert!(android(&mut reg, "d-1", "token-3"));
    let d = reg.get("d-1").unwrap();
    assert!(d.push.is_none() && d.android_push.is_some());
    assert_eq!(state_view(d, None).prefs, prefs);
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(
        !text.contains("127.0.0.1:9") && text.contains("token-3"),
        "{text}"
    );
    // A browser registering over a browser reports nothing replaced.
    let sub = check_with(
        "http://127.0.0.1:9/a",
        &ua.p256dh(),
        &b64(&ua.auth),
        LOOPBACK,
    )
    .unwrap();
    assert!(subscribe(&mut reg, "d-1", sub.clone()).unwrap());
    assert!(!subscribe(&mut reg, "d-1", sub).unwrap());

    // Unsubscribing clears whichever transport; the file is as it was for a browser-less device.
    assert!(android(&mut reg, "d-1", "token-4"));
    unsubscribe(&mut reg, "d-1").unwrap();
    assert!(!reg.get("d-1").unwrap().has_push());
    assert!(
        !std::fs::read_to_string(&path)
            .unwrap()
            .contains("android_push")
    );
    // Revoke and unpair remove the record: nothing of the registration stays on disk.
    assert!(android(&mut reg, "d-1", "token-5"));
    reg.remove("d-1").unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(
        !text.contains("token-5") && !text.contains(&b64(&google::KEY)),
        "{text}"
    );
}

#[test]
fn android_push_follows_the_same_gates_as_web_push_and_is_sealed() {
    let server = google::Server::google(|_, _| google::Reply::ok());
    let web = Receiver::start(&[201]);
    let ua = Ua::new();
    let dir = tempfile::tempdir().unwrap();
    let mut reg = registry(
        dir.path(),
        &["d-want", "d-off", "d-here", "d-titles", "d-web", "d-none"],
    );
    for id in ["d-want", "d-off", "d-here", "d-titles"] {
        android(&mut reg, id, &format!("tok-{id}"));
    }
    subscribe_ua(&mut reg, "d-web", &web.endpoint("web"), &ua);
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
    // Without an FCM account a gateway cannot send to Android apps.
    assert!(!push().android_configured());
    let mut push = android_push(&server);
    assert!(push.android_configured());

    push.offer(
        reg.devices(),
        |id| id == "d-here",
        &offer(MobileKind::Attention, &Opens::Session("sess-9".to_owned())),
    );
    // d-off turned attention off, d-here is looking at the app, d-none has no subscription.
    let mut got: Vec<google::Request> = (0..2).map(|_| server.next_message()).collect();
    got.sort_by_key(|r| r.json()["message"]["token"].as_str().unwrap().to_owned());
    assert_eq!(got[0].json()["message"]["token"], "tok-d-titles");
    assert_eq!(got[1].json()["message"]["token"], "tok-d-want");
    let plain = |r: &google::Request| -> serde_json::Value {
        json(&google::open(&google::KEY, DESKTOP, &r.json()["message"]["data"]).unwrap())
    };
    let titled = plain(&got[0]);
    assert_eq!(
        titled["notification"]["body"],
        "fix the parser needs your attention"
    );
    let generic = plain(&got[1]);
    assert_eq!(
        generic["notification"]["body"],
        "A session needs your attention"
    );
    for p in [&titled, &generic] {
        assert_eq!(p["web_push"], 8030);
        assert_eq!(
            p["notification"]["navigate"],
            "https://relay.example/#s=sess-9"
        );
        assert_eq!(p["notification"]["tag"], topic("sess-9"));
        let expires = p["expires_unix_ms"].as_u64().unwrap();
        let ttl = u64::try_from(fcm::TTL.as_millis()).unwrap();
        assert!(
            (now_ms() + ttl - 5_000..=now_ms() + ttl).contains(&expires),
            "{expires}"
        );
    }
    // The browser device got its Web Push, once; Google saw nothing of it.
    assert_eq!(web.next().path, "/push/web");
    web.quiet(Duration::from_millis(300));
    server.quiet(Duration::from_millis(300));

    // A kind the device turned off sends nothing; the others still go.
    push.offer(
        reg.devices(),
        |_| false,
        &offer(MobileKind::Ended, &Opens::Session("sess-9".to_owned())),
    );
    let mut tokens: Vec<String> = (0..4)
        .map(|_| {
            server.next_message().json()["message"]["token"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect();
    tokens.sort();
    assert_eq!(
        tokens,
        ["tok-d-here", "tok-d-off", "tok-d-titles", "tok-d-want"]
    );
}

#[test]
fn a_canonical_event_reaches_fcm_once_and_a_gateway_without_an_account_sends_nothing() {
    let server = google::Server::google(|_, _| google::Reply::ok());
    let dir = tempfile::tempdir().unwrap();
    let mut reg = registry(dir.path(), &["d-1"]);
    android(&mut reg, "d-1", "tok-1");
    let key = SessionTarget {
        epoch: ServiceEpoch::current(),
        key: SessionKey::tmux("omp-deck", "p1"),
        generation: 1,
    };
    let mut center = NotificationCenter::new(Arc::new(|| {}));
    let effects = one_finished_turn(&mut center, &key);

    // No FCM account on this gateway: the registration is kept, nothing is sent.
    let mut unconfigured = push();
    apply(&mut unconfigured, &reg, &[], effects.clone(), "sess-1");
    server.quiet(Duration::from_millis(300));
    assert!(reg.get("d-1").unwrap().android_push.is_some());

    let mut push = android_push(&server);
    apply(&mut push, &reg, &[], effects, "sess-1");
    let got = server.next_message();
    assert_eq!(got.path, "/v1/projects/ompdeck-test/messages:send");
    let body = got.json();
    assert_eq!(body["message"]["android"]["ttl"], "3600s");
    let plain = json(&google::open(&google::KEY, DESKTOP, &body["message"]["data"]).unwrap());
    assert_eq!(plain["notification"]["body"], "A session finished");
    assert_eq!(
        plain["notification"]["navigate"],
        "https://relay.example/#s=sess-1"
    );
    // One canonical event, one push.
    server.quiet(Duration::from_millis(500));
}

#[test]
fn an_unregistered_token_is_deleted_exactly_and_never_a_newer_one() {
    let server =
        google::Server::google(|_, _| google::Reply::fcm_error(404, "NOT_FOUND", "UNREGISTERED"));
    let dir = tempfile::tempdir().unwrap();
    let mut reg = registry(dir.path(), &["d-1"]);
    android(&mut reg, "d-1", "dead-token");
    let mut push = android_push(&server);
    push.offer(
        reg.devices(),
        |_| false,
        &offer(MobileKind::TurnFinished, &Opens::Session("s".to_owned())),
    );
    server.next_message();
    let gone = (0..250)
        .find_map(|_| {
            let g = push.take_gone_android();
            if g.is_empty() {
                std::thread::sleep(Duration::from_millis(20));
                None
            } else {
                Some(g)
            }
        })
        .expect("the UNREGISTERED is reported");
    assert_eq!(gone.len(), 1);
    assert_eq!(
        (gone[0].device.as_str(), gone[0].token.as_str()),
        ("d-1", "dead-token")
    );
    // Not retried, and reported once.
    server.quiet(Duration::from_millis(300));
    assert!(push.take_gone_android().is_empty());

    // The phone registered a refreshed token or a new key meanwhile: that one stays.
    assert!(android(&mut reg, "d-1", "new-token"));
    assert!(!forget_android(&mut reg, &gone[0]).unwrap());
    let rekeyed = check_android("dead-token", &b64(&[9; 32]), DESKTOP, DESKTOP, true).unwrap();
    subscribe_android(&mut reg, "d-1", rekeyed).unwrap();
    assert!(!forget_android(&mut reg, &gone[0]).unwrap());
    assert!(reg.get("d-1").unwrap().android_push.is_some());
    // The exact registration is deleted, and the deletion is durable. A browser subscription is
    // never touched by it.
    assert!(android(&mut reg, "d-1", "dead-token"));
    assert!(forget_android(&mut reg, &gone[0]).unwrap());
    assert!(!reg.get("d-1").unwrap().has_push());
    let reloaded = Registry::load(&dir.path().join("devices.json")).unwrap();
    assert!(!reloaded.get("d-1").unwrap().has_push());
    let ua = Ua::new();
    subscribe_ua(&mut reg, "d-1", "http://127.0.0.1:9/a", &ua);
    assert!(!forget_android(&mut reg, &gone[0]).unwrap());
    assert!(reg.get("d-1").unwrap().push.is_some());
}

#[test]
fn the_secrets_dir_service_account_turns_android_push_on() {
    let dir = tempfile::tempdir().unwrap();
    let secrets = dir.path().join("secrets");
    std::fs::create_dir_all(&secrets).unwrap();
    std::fs::write(
        secrets.join(fcm::CREDENTIALS_FILE),
        serde_json::json!({
            "type": "service_account",
            "project_id": google::PROJECT,
            "client_email": "gateway@ompdeck-test.iam.gserviceaccount.com",
            "private_key": include_str!("fcm/test-rsa-key.pem"),
        })
        .to_string(),
    )
    .unwrap();
    assert!(Push::open(&secrets, Arc::new(|| {})).android_configured());
}

#[test]
fn startup_public_android_options_are_cached_and_never_change_device_registrations() {
    let dir = tempfile::tempdir().unwrap();
    let secrets = dir.path().join("secrets");
    std::fs::create_dir_all(&secrets).unwrap();
    std::fs::write(
        secrets.join(fcm::CREDENTIALS_FILE),
        serde_json::json!({
            "type": "service_account",
            "project_id": google::PROJECT,
            "client_email": "gateway@ompdeck-test.iam.gserviceaccount.com",
            "private_key": include_str!("fcm/test-rsa-key.pem"),
        })
        .to_string(),
    )
    .unwrap();
    let public = serde_json::json!({
        "project_info": {
            "project_id": google::PROJECT,
            "project_number": "123456789012",
        },
        "client": [{
            "client_info": {
                "mobilesdk_app_id": "1:123456789012:android:0123456789abcdef",
                "android_client_info": {"package_name": "io.github.lavinhoque33.ompdeck"},
            },
            "api_key": [{"current_key": "AIzaSyA-0123456789abcdefghijklmnopqrstuv"}],
        }],
    });
    let mut source = public.clone();
    source["private_key"] = "MUST NOT LEAVE DESKTOP".into();
    source["account_path"] = "/private/credentials/fcm-service-account.json".into();
    std::fs::write(secrets.join(android_config::FILE), source.to_string()).unwrap();
    let push = Push::open(&secrets, Arc::new(|| {}));
    assert!(push.android_configured());
    let mut reg = registry(dir.path(), &["d-1", "d-2", "d-3"]);
    let empty = state_view(reg.get("d-1").unwrap(), push.android_config());
    assert!(!empty.subscribed);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(empty.android_config.as_deref().unwrap())
            .unwrap(),
        public
    );
    // Configuration is startup-only: subsequent feeds do not reread a changed/deleted file.
    std::fs::remove_file(secrets.join(android_config::FILE)).unwrap();
    assert_eq!(
        state_view(reg.get("d-1").unwrap(), push.android_config()),
        empty
    );
    assert!(android(&mut reg, "d-1", "existing-token"));
    let prefs = PushPrefs {
        attention: false,
        show_titles: true,
        ..PushPrefs::default()
    };
    set_prefs(&mut reg, "d-1", prefs).unwrap();
    let ua = Ua::new();
    subscribe_ua(&mut reg, "d-2", "http://127.0.0.1:9/browser", &ua);
    set_prefs(&mut reg, "d-2", prefs).unwrap();
    let stored = std::fs::read(dir.path().join("devices.json")).unwrap();
    for id in ["d-1", "d-2", "d-3"] {
        let device = reg.get(id).unwrap();
        let before = state_view(device, None);
        let offered = state_view(device, push.android_config());
        assert_eq!(offered.subscribed, before.subscribed);
        assert_eq!(offered.prefs, before.prefs);
        assert_eq!(offered.since_unix_ms, before.since_unix_ms);
        assert_eq!(offered.android_config, empty.android_config);
        let encoded = serde_json::to_string(&offered).unwrap();
        assert!(!encoded.contains("existing-token"));
        assert!(!encoded.contains("private_key"));
        assert!(!encoded.contains("/private/"));
    }
    assert_eq!(
        std::fs::read(dir.path().join("devices.json")).unwrap(),
        stored
    );
    let stored = String::from_utf8(stored).unwrap();
    assert!(!stored.contains("android_config"));
    assert!(!stored.contains("mobilesdk_app_id"));
    let push = push.with_fcm(None);
    assert!(!push.android_configured());
    assert!(push.android_config().is_none());
}

#[test]
fn agent_messages_use_attention_preferences_high_urgency_and_private_text() {
    let rx = Receiver::start(&[201]);
    let dir = tempfile::tempdir().unwrap();
    let mut reg = registry(dir.path(), &["agent-private", "agent-titled", "agent-off"]);
    let uas: Vec<Ua> = (0..3).map(|_| Ua::new()).collect();
    for (ua, id) in uas
        .iter()
        .zip(["agent-private", "agent-titled", "agent-off"])
    {
        subscribe_ua(&mut reg, id, &rx.endpoint(id), ua);
    }
    set_prefs(
        &mut reg,
        "agent-titled",
        PushPrefs {
            show_titles: true,
            ..PushPrefs::default()
        },
    )
    .unwrap();
    set_prefs(
        &mut reg,
        "agent-off",
        PushPrefs {
            attention: false,
            ..PushPrefs::default()
        },
    )
    .unwrap();
    let mut push = push();
    let opens = Opens::Session("agent-session".into());
    push.offer(
        reg.devices(),
        |_| false,
        &Offer {
            title: "fix the parser: Choose a path",
            ..offer(MobileKind::AgentMessage, &opens)
        },
    );
    let mut got = vec![rx.next(), rx.next()];
    rx.quiet(Duration::from_millis(500));
    got.sort_by(|a, b| a.path.cmp(&b.path));
    assert_eq!(got[0].path, "/push/agent-private");
    assert_eq!(got[1].path, "/push/agent-titled");
    assert!(got.iter().all(|r| r.headers["urgency"] == "high"));
    let titled = json(&uas[1].decrypt(got.pop().unwrap().body));
    assert_eq!(
        titled["notification"]["body"],
        "fix the parser: Choose a path"
    );
    let private = json(&uas[0].decrypt(got.pop().unwrap().body));
    assert_eq!(
        private["notification"]["body"],
        "An agent sent you a message"
    );
}

#[test]
fn agent_message_payload_keeps_shape_and_cuts_unicode_titles() {
    let title = format!("session: {}", "é".repeat(TITLE_CHARS));
    let encoded = payload(MobileKind::AgentMessage, Some(&title), ORIGIN, None, "tag").unwrap();
    let value = json(&encoded);
    assert_eq!(value["notification"]["body"], cut(&title, TITLE_CHARS));
    assert_eq!(value["notification"]["title"], "OmpDeck");
    assert_eq!(value["notification"].as_object().unwrap().len(), 5);
    let prefs = PushPrefs {
        attention: false,
        ..PushPrefs::default()
    };
    assert!(!wants(&prefs, MobileKind::AgentMessage));
    assert!(wants(&prefs, MobileKind::TurnFinished));
    assert!(wants(&prefs, MobileKind::Ended));
}
