//! A live Google → phone smoke of the gateway's real FCM sender. Ignored by default: it needs a
//! real Firebase service account and a phone registration, and it sends one real notification.
//!
//! ```text
//! OMP_DECK_FCM_CREDENTIALS=/path/to/service-account.json \
//! OMPDECK_ANDROID_PUSH_REGISTRATION=/path/to/registration.json \
//! [OMPDECK_ANDROID_PUSH_CONFIRMED=/path/that/main/touches]   # ends the wait early
//! [OMPDECK_ANDROID_PUSH_WAIT_SECS=60]                        # default 60, at most 120
//! [OMPDECK_ANDROID_PUSH_ORIGIN=https://…]                    # the PWA origin in `navigate`
//! [OMPDECK_ANDROID_PUSH_SESSION=<existing session id>]       # optional meaningful tap destination
//! cargo test -p deck-service --test android_push_live -- --ignored --nocapture
//! ```
//!
//! `registration.json` is `{"token": "...", "key": "<base64url 32 bytes>", "desktop": "<64 hex>"}`
//! as the native bridge's `pushSubscription` returns it. The test registers that on a throwaway
//! device and offers one Attention notification through [`Push`] exactly as the engine does
//! (the same sealing, envelope and endpoints), then keeps the sender alive until the
//! confirmation file appears or a line arrives on stdin. The sender has no success callback:
//! Google's answer is in the log (`FCM accepted it` at debug, a warning or error otherwise).
//! Proof of delivery is the notification on the phone; only confirm after observing it.
//! Nothing secret is printed. UNREGISTERED or no confirmation before the deadline fails.

use std::io::BufRead as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use deck_remote::api::PushPrefs;
use deck_service::gateway::protocol::Permissions;
use deck_service::gateway::push::{self, Offer, Opens, Policy, Push, fcm};
use deck_service::gateway::remote::registry::Device;
use deck_service::notifications::MobileKind;

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

#[test]
#[ignore = "live: sends a real FCM notification (OMP_DECK_FCM_CREDENTIALS, OMPDECK_ANDROID_PUSH_REGISTRATION)"]
fn a_real_attention_notification_reaches_the_phone() {
    let _ = env_logger::Builder::new()
        .filter_level(log::LevelFilter::Info)
        .filter_module("deck_service::gateway::push", log::LevelFilter::Debug)
        .try_init();

    let registration = env_path("OMPDECK_ANDROID_PUSH_REGISTRATION")
        .expect("OMPDECK_ANDROID_PUSH_REGISTRATION must name the registration JSON");
    let value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&registration).expect("readable registration"))
            .expect("registration is JSON");
    let field = |name: &str| -> String {
        value[name]
            .as_str()
            .unwrap_or_else(|| panic!("registration has no string `{name}`"))
            .to_owned()
    };
    let (token, key, desktop) = (field("token"), field("key"), field("desktop"));

    // The service account is read the way the gateway reads it at startup (the env var wins; the
    // directory is only the fallback location of `fcm-service-account.json`).
    let dir = tempfile::tempdir().unwrap();
    let config = fcm::load(dir.path())
        .expect("OMP_DECK_FCM_CREDENTIALS must name a valid Firebase service account");

    // This test has no gateway identity, so the registration's desktop is taken as this one.
    let mut android =
        push::check_android(&token, &key, &desktop, &desktop, true).expect("a valid registration");
    android.prefs = PushPrefs {
        show_titles: true,
        ..PushPrefs::default()
    };
    let device = Device {
        id: "live-smoke".to_owned(),
        name: "live smoke".to_owned(),
        platform: "android".to_owned(),
        public: [1; 32],
        token_hash: [2; 32],
        permissions: Permissions::default(),
        last_ts: 1,
        created_unix_ms: 1,
        last_seen_unix_ms: 1,
        push: None,
        android_push: Some(android),
    };

    let mut push = Push::with_key(
        None,
        Policy {
            allow_loopback: false,
        },
        None,
        Arc::new(|| {}),
    )
    .with_fcm(Some(config));
    assert!(push.android_configured());

    let confirmed = env_path("OMPDECK_ANDROID_PUSH_CONFIRMED");
    assert!(
        !confirmed.as_deref().is_some_and(std::path::Path::exists),
        "remove the old confirmation file before sending a new notification"
    );
    let origin = std::env::var("OMPDECK_ANDROID_PUSH_ORIGIN")
        .unwrap_or_else(|_| "https://relay.example".to_owned());
    let opens = Opens::Session(
        std::env::var("OMPDECK_ANDROID_PUSH_SESSION")
            .unwrap_or_else(|_| "live-smoke-session".to_owned()),
    );
    push.offer(
        std::slice::from_ref(&device),
        |_| false,
        &Offer {
            kind: MobileKind::Attention,
            title: "OmpDeck live FCM smoke",
            opens: Some(&opens),
            fallback: "live-smoke",
            origin: &origin,
            desktop: &desktop,
        },
    );
    println!("offered one Attention notification; waiting for the phone (sender stays alive)");

    // A newline on stdin ends the wait too; stdin at EOF (no terminal) does not.
    let (tx, line) = mpsc::channel();
    std::thread::spawn(move || {
        let mut text = String::new();
        if matches!(std::io::stdin().lock().read_line(&mut text), Ok(n) if n > 0) {
            let _ = tx.send(());
        }
    });
    let wait = std::env::var("OMPDECK_ANDROID_PUSH_WAIT_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(60)
        .min(120);
    let deadline = Instant::now() + Duration::from_secs(wait);
    while Instant::now() < deadline {
        let unregistered = push.take_gone_android();
        assert!(
            unregistered.is_empty(),
            "FCM answered UNREGISTERED: the registration's token is dead"
        );
        if confirmed.as_deref().is_some_and(std::path::Path::exists) {
            println!("confirmation file seen");
            return;
        }
        if line.recv_timeout(Duration::from_millis(200)).is_ok() {
            println!("stdin line seen");
            return;
        }
    }
    panic!("no observed phone delivery was confirmed within {wait}s");
}
