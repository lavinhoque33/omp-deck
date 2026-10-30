//! `omp-deck-relay enroll/list/remove` on a temp state dir.

use std::process::{Command, Output};

use deck_remote::relay::{self, RoomKey};

fn relay(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_omp-deck-relay"))
        .args(args)
        .output()
        .unwrap()
}

fn stdout(o: &Output) -> String {
    String::from_utf8(o.stdout.clone()).unwrap()
}

#[test]
fn enroll_list_remove() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let state = state.to_str().unwrap();

    let out = relay(&["enroll", "--state-dir", state, "--label", "home lab"]);
    assert!(out.status.success(), "{out:?}");
    let text = stdout(&out);
    let field = |name: &str| {
        text.lines()
            .find_map(|l| l.strip_prefix(name))
            .unwrap()
            .trim()
            .to_owned()
    };
    let id = field("room id:");
    let key = RoomKey::parse(&field("room key:")).unwrap();
    assert_eq!(relay::room_id(&key.room), id);
    // Only the hash of the secret is stored.
    let stored = std::fs::read_to_string(dir.path().join("state/rooms.json")).unwrap();
    assert!(stored.contains(&id));
    assert!(!stored.contains(&field("room key:")));

    let out = relay(&["enroll", "--state-dir", state]);
    assert!(out.status.success());
    let list = stdout(&relay(&["list", "--state-dir", state]));
    assert_eq!(list.lines().count(), 2, "{list}");
    let line = list.lines().find(|l| l.starts_with(&id)).unwrap();
    assert!(line.ends_with("home lab"), "{line}");
    assert!(line.contains(" UTC"), "{line}");

    let out = relay(&["remove", "--state-dir", state, &id]);
    assert!(out.status.success(), "{out:?}");
    let list = stdout(&relay(&["list", "--state-dir", state]));
    assert_eq!(list.lines().count(), 1);
    assert!(!list.contains(&id));
    let out = relay(&["remove", "--state-dir", state, &id]);
    assert_eq!(out.status.code(), Some(1));
}

#[test]
fn usage_errors_help_and_version() {
    assert_eq!(relay(&["list"]).status.code(), Some(2));
    assert_eq!(relay(&["bogus"]).status.code(), Some(2));
    assert_eq!(
        relay(&["enroll", "--state-dir", "x", "--listen", "127.0.0.1:1"])
            .status
            .code(),
        Some(2)
    );
    assert_eq!(
        relay(&["serve", "--state-dir", "x", "--max-connections", "0"])
            .status
            .code(),
        Some(2)
    );
    assert_eq!(
        relay(&["serve", "--state-dir", "x", "--listen", "nowhere"])
            .status
            .code(),
        Some(2)
    );
    let help = relay(&["--help"]);
    assert!(help.status.success());
    assert!(String::from_utf8_lossy(&help.stderr).contains("usage: omp-deck-relay"));
    let version = relay(&["--version"]);
    assert!(stdout(&version).starts_with("omp-deck-relay "));
}

#[test]
fn forwarded_for_flag_combinations() {
    let fixtures = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");
    let (cert, key) = (
        format!("{fixtures}/cert.pem"),
        format!("{fixtures}/key.pem"),
    );
    let serve = |extra: &[&str]| {
        let mut args = vec!["serve", "--state-dir", "x", "--listen", "127.0.0.1:0"];
        args.extend_from_slice(extra);
        relay(&args)
    };
    // In-process TLS: clients connect directly, no proxy header to trust.
    let out = serve(&[
        "--trust-forwarded-for",
        "--tls-cert",
        &cert,
        "--tls-key",
        &key,
    ]);
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stderr).contains("--tls-cert"));
    // --trusted-proxy needs the flag and an address or CIDR range.
    assert_eq!(
        serve(&["--trusted-proxy", "10.88.0.1"]).status.code(),
        Some(2)
    );
    for bad in ["10.88.0.0/33", "proxy.example", "10.88.0.1/"] {
        let out = serve(&["--trust-forwarded-for", "--trusted-proxy", bad]);
        assert_eq!(out.status.code(), Some(2), "{bad}: {out:?}");
    }
    assert_eq!(
        relay(&["list", "--state-dir", "x", "--trusted-proxy", "10.88.0.1"])
            .status
            .code(),
        Some(2)
    );
}
