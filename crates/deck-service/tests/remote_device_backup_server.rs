//! A device and a backup server whose login fails: the SFTP key file cannot be read. The
//! failure reaches the device on every path — the `backup_list` query's error, a `backup_run`
//! outcome and the `backup` feed's `error` (an automatic backup's failure) — in fixed words,
//! never with the key file, the login user or the server folder (the desktop keeps the detail).
//!
//! The server is an in-process SSH server with a fixed host key (pinned in the saved server, so
//! the dial gets as far as loading the key); private HOME/XDG tree.

mod support;

use std::time::Duration;

use deck_core::backup::server::{Protocol, ServerAccount, ServerAuth, ServerConfig, ServerStore};
use deck_remote::api::{
    ApiError, BackupKind, Event, Feed, FeedValue, Op, OutcomeView, ReplyResult, Resp,
};
use deck_service::gateway::client::Link;
use deck_service::gateway::protocol::{Mutation, Permissions, RelayLink, SecretText};
use russh::keys::decode_secret_key;
use support::device::{
    Device, OMP_CONFIG, Relay, STEP, Sandbox, accepted, event, pair, run, wait_for, wait_mobile,
};

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

/// What the failure must never show a device.
const USER: &str = "alice_marker";
const FOLDER: &str = "folder_marker";
const KEY_FILE: &str = "id_key_marker";
const FIXED: &str = "Could not log in to the server (key file unreadable)";

struct Ssh;

impl russh::server::Handler for Ssh {
    type Error = russh::Error;
}

/// An SSH server on an ephemeral loopback port (its handshake is all the dial needs); runs for
/// the rest of the test process.
fn ssh_server() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");
    let port = listener.local_addr().expect("addr").port();
    let config = std::sync::Arc::new(russh::server::Config {
        keys: vec![decode_secret_key(HOST_KEY, None).expect("host key")],
        ..russh::server::Config::default()
    });
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        rt.block_on(async move {
            let listener = tokio::net::TcpListener::from_std(listener).expect("listener");
            while let Ok((socket, _)) = listener.accept().await {
                let config = config.clone();
                tokio::spawn(async move {
                    if let Ok(session) = russh::server::run_stream(config, socket, Ssh).await {
                        let _ = session.await;
                    }
                });
            }
        });
    });
    port
}

fn never_shows_the_login(what: &str, shown: &impl std::fmt::Debug) {
    let text = format!("{shown:?}");
    for secret in [USER, FOLDER, KEY_FILE] {
        assert!(!text.contains(secret), "{what} shows {secret}: {text}");
    }
}

#[test]
fn a_server_login_failure_reaches_a_device_without_the_key_user_or_folder() {
    let sb = Sandbox::new("rdbs", OMP_CONFIG);
    sb.report_failures();
    let port = ssh_server();
    let key = sb.root.join("home/.ssh").join(KEY_FILE);
    // Saved before the owner starts: the server destination, an SFTP login by a key file that
    // does not exist.
    std::fs::write(
        &sb.profile.settings,
        br#"{"gateway_mode":"Background","backup_destination":"Server"}"#,
    )
    .expect("settings");
    ServerStore::at(sb.root.join("config/omp-deck/backup-server.json"))
        .save(&ServerAccount {
            config: ServerConfig {
                protocol: Protocol::Sftp,
                host: "127.0.0.1".to_owned(),
                port,
                auth: ServerAuth::Key {
                    user: USER.to_owned(),
                    key_path: key.clone(),
                    passphrase: None,
                },
                folder: FOLDER.to_owned(),
                pinned: Some(HOST_FP.to_owned()),
            },
            last_backup: None,
            synced_fingerprint: None,
        })
        .expect("saved server");
    let relay = Relay::start(sb.root.join("relay"));
    let phone = {
        let client = sb.client();
        wait_for("the gateway link", Duration::from_secs(30), || {
            matches!(client.link(), Link::Up(_)).then_some(())
        });
        run(
            &client,
            Mutation::RelaySet {
                url: relay.url(),
                room_key: SecretText::new(relay.key.encode()),
            },
        )
        .expect("relay set");
        wait_mobile(&client, "the relay link", |m| m.link == RelayLink::Up);
        pair(
            &client,
            &relay,
            "Phone",
            Permissions {
                control: true,
                manage: true,
            },
        )
    };
    let mut a = Device::connect(&relay, &phone, "Phone", true);
    let mut seq = a.welcome.next_seq;

    // -- the feed names the server by kind and label, nothing of its login --
    let r = a.call(
        &mut [],
        None,
        Op::Subscribe {
            feed: Feed::Backup {},
        },
    );
    assert!(matches!(r, ReplyResult::Ok(Resp::Unit {})), "{r:?}");
    let first = a.take(&mut [], "the backup feed", STEP, |r| match event(r)? {
        Event::Feed {
            value: v @ FeedValue::Backup { .. },
            ..
        } => Some(v),
        _ => None,
    });
    match &first {
        FeedValue::Backup {
            destination,
            available,
            connected,
            account,
            ..
        } => {
            assert_eq!(destination.kind, BackupKind::Sftp);
            assert_eq!(destination.label, format!("sftp://127.0.0.1:{port}"));
            assert!(*available && *connected, "{first:?}");
            assert_eq!(*account, None);
        }
        other => panic!("{other:?}"),
    }
    never_shows_the_login("the backup feed", &first);

    // -- the query's error --
    let r = a.call(&mut [], None, Op::BackupList {});
    match &r {
        ReplyResult::Err(ApiError::Failed { message }) => {
            assert!(message.starts_with(FIXED), "{message}");
        }
        other => panic!("backup_list: {other:?}"),
    }
    never_shows_the_login("the backup_list error", &r);

    // -- a backup run's outcome --
    let run_seq = seq;
    seq += 1;
    assert!(!accepted(a.call(&mut [], Some(run_seq), Op::BackupRun {})));
    let outcome = loop {
        match a.outcome(&mut [], run_seq, Duration::from_secs(60)) {
            OutcomeView::Running {} => {}
            other => break other,
        }
    };
    match &outcome {
        OutcomeView::Failed { message } => assert!(message.starts_with(FIXED), "{message}"),
        other => panic!("backup_run: {other:?}"),
    }
    never_shows_the_login("the backup_run outcome", &outcome);
    // The same outcome asked again later.
    let again = a.call(&mut [], None, Op::Outcome { seq: run_seq });
    never_shows_the_login("the recalled outcome", &again);
    assert!(
        format!("{again:?}").contains(FIXED),
        "the recalled outcome is not in device terms: {again:?}"
    );
    let _ = seq;

    // -- the feed's last error: the automatic backup fails the same way --
    let error = a.take(
        &mut [],
        "the feed's automatic backup error",
        Duration::from_secs(150),
        |r| match event(r)? {
            Event::Feed {
                value: FeedValue::Backup { error: Some(e), .. },
                ..
            } => Some(e),
            _ => None,
        },
    );
    assert!(error.starts_with(FIXED), "{error}");
    never_shows_the_login("the feed's error", &error);
}
