//! End-to-end tests against real `omp-deck-host` daemons (Linux). Every test uses its own
//! namespace `omp-deck-test-<pid>-<n>` under a temporary `XDG_RUNTIME_DIR`; daemons are
//! killed afterwards.
#![cfg(target_os = "linux")]

mod common;

use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::time::Duration;

use alacritty_terminal::term::TermMode;
use common::{Client, TestNs, env, params, scratch, wait_until};
use deck_host::{Error, HostEvent, SessionSpec};

fn spec(name: &str, program: &str, args: &[&str]) -> SessionSpec {
    SessionSpec {
        name: name.into(),
        program: program.into(),
        args: args.iter().map(|a| (*a).to_owned()).collect(),
        cwd: env().root.clone(),
        env_set: vec![
            ("HOME".into(), env().root.join("home").display().to_string()),
            ("PS1".into(), "$ ".into()),
            ("TERM".into(), "xterm-256color".into()),
        ],
        env_remove: vec!["ENV".into(), "BASH_ENV".into()],
    }
}

fn shell(name: &str) -> SessionSpec {
    spec(name, "/bin/sh", &[])
}

fn pid_alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
        && !std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|s| s.contains(") Z "))
}

#[test]
fn input_round_trip_detach_and_reattach_with_same_screen_and_cursor() {
    let ns = TestNs::new();
    let mut c = Client::create(&ns, &shell("rt"), 80, 24);
    c.type_line("printf 'hello-%s\\n' world");
    c.wait_output("hello-world");
    c.wait_output("$ ");
    c.settle(Duration::from_millis(300));
    let cursor = c.cursor();
    let screen = c.screen();
    assert!(screen.contains("hello-world"), "{screen}");

    c.tx.detach().unwrap();
    assert!(matches!(c.next(), HostEvent::Closed));
    assert_eq!(ns.0.list(), ["rt"]);
    wait_until("detached state", || !ns.0.info("rt").unwrap().attached);

    let again = Client::attach(&ns, "rt", 80, 24);
    assert_eq!(again.screen(), screen);
    assert_eq!(again.cursor(), cursor);
    let info = ns.0.info("rt").unwrap();
    assert!(info.attached);
    assert_eq!((info.cols, info.rows), (80, 24));
    assert!(info.tty.as_deref().is_some_and(|t| t.starts_with("/dev/")));
    assert_eq!(info.argv, ["/bin/sh"]);
}

#[test]
fn alternate_screen_state_survives_reattach() {
    let ns = TestNs::new();
    let mut c = Client::create(&ns, &shell("alt"), 60, 20);
    c.type_line("echo primary-marker");
    c.wait_output("primary-marker\r\n");
    c.type_line("printf '\\033[?1049h\\033[H\\033[?2004halt-marker'; read x");
    c.wait_output("alt-marker");
    c.settle(Duration::from_millis(300));
    assert!(c.term.mode().contains(TermMode::ALT_SCREEN));
    let cursor = c.cursor();
    drop(c);

    let mut again = Client::attach(&ns, "alt", 60, 20);
    assert!(
        again
            .term
            .mode()
            .contains(TermMode::ALT_SCREEN | TermMode::BRACKETED_PASTE)
    );
    assert!(
        again.screen().starts_with("alt-marker"),
        "{}",
        again.screen()
    );
    assert_eq!(again.cursor(), cursor);
    // The primary screen comes back with its content when the program leaves.
    again.tx.input(b"\r").unwrap();
    again.type_line("printf '\\033[?1049l'; echo left-alt");
    again.wait_output("left-alt\r\n");
    again.settle(Duration::from_millis(200));
    assert!(!again.term.mode().contains(TermMode::ALT_SCREEN));
    assert!(
        again.screen().contains("primary-marker"),
        "{}",
        again.screen()
    );
}

#[test]
fn resize_reaches_the_child() {
    let ns = TestNs::new();
    let mut c = Client::create(&ns, &shell("rs"), 80, 24);
    c.tx.resize(101, 33, 9, 18).unwrap();
    c.type_line("stty size");
    c.wait_output("33 101");
    assert_eq!(
        (ns.0.info("rs").unwrap().cols, ns.0.info("rs").unwrap().rows),
        (101, 33)
    );
}

#[test]
fn queries_are_answered_while_detached() {
    let ns = TestNs::new();
    let out = scratch(&format!("cpr-{}", std::process::id()));
    let _ = std::fs::remove_file(&out);
    let script = format!(
        "stty raw -echo; printf 'abc\\033[6n'; r=$(dd bs=1 count=6 2>/dev/null); \
         printf '\\033[c'; d=$(dd bs=1 count=5 2>/dev/null); \
         printf '%s|%s' \"$r\" \"$d\" | od -An -c | tr -s ' ' > {0}.tmp; mv {0}.tmp {0}; sleep 100",
        out.display()
    );
    deck_host::start(&ns.0, &spec("q", "/bin/sh", &["-c", &script]), 80, 24).unwrap();
    wait_until("query replies", || out.exists());
    let got = std::fs::read_to_string(&out).unwrap();
    assert_eq!(got.trim(), r"033 [ 1 ; 4 R | 033 [ ? 6 c");
}

#[test]
fn second_attach_displaces_the_first() {
    let ns = TestNs::new();
    let mut first = Client::create(&ns, &shell("dp"), 80, 24);
    let mut second = Client::attach(&ns, "dp", 80, 24);
    assert_eq!(first.next(), HostEvent::Displaced);
    assert_eq!(first.next(), HostEvent::Closed);
    second.type_line("echo from-second");
    second.wait_output("from-second\r\n");
    assert!(ns.0.info("dp").unwrap().attached);
}

#[test]
fn kill_removes_session_and_process_tree() {
    let ns = TestNs::new();
    let pid_file = scratch(&format!("gpid-{}", std::process::id()));
    let _ = std::fs::remove_file(&pid_file);
    let script = format!(
        "sleep 1000 & echo $! > {0}.tmp; mv {0}.tmp {0}; wait",
        pid_file.display()
    );
    deck_host::start(&ns.0, &spec("k", "/bin/sh", &["-c", &script]), 80, 24).unwrap();
    wait_until("grandchild pid", || pid_file.exists());
    let grandchild: u32 = std::fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let info = ns.0.info("k").unwrap();
    assert!(pid_alive(grandchild) && pid_alive(info.child_pid) && pid_alive(info.daemon_pid));

    ns.0.kill("k").unwrap();
    assert!(!ns.0.has("k"));
    assert!(ns.0.list().is_empty());
    assert!(!ns.dir().join("k.sock").exists());
    wait_until("process tree gone", || {
        !pid_alive(grandchild) && !pid_alive(info.child_pid) && !pid_alive(info.daemon_pid)
    });
    // Killing a session that is gone is not an error.
    ns.0.kill("k").unwrap();
}

#[test]
fn attached_kill_reports_exit() {
    let ns = TestNs::new();
    let mut c = Client::create(&ns, &shell("ak"), 80, 24);
    c.tx.kill().unwrap();
    loop {
        match c.next() {
            HostEvent::Exited(_) => break,
            HostEvent::Output(_) => {}
            other => panic!("unexpected {other:?}"),
        }
    }
    wait_until("endpoint gone", || !ns.0.has("ak"));
}

#[test]
fn child_exit_reports_code_and_removes_endpoint() {
    let ns = TestNs::new();
    let mut c = Client::create(
        &ns,
        &spec("ex", "/bin/sh", &["-c", "echo bye; sleep 0.3; exit 7"]),
        80,
        24,
    );
    loop {
        match c.next() {
            HostEvent::Exited(code) => {
                assert_eq!(code, Some(7));
                break;
            }
            HostEvent::Output(_) => {}
            other => panic!("unexpected {other:?}"),
        }
    }
    assert!(c.screen().contains("bye"), "{}", c.screen());
    assert_eq!(c.next(), HostEvent::Closed);
    assert!(!ns.0.has("ex"));
    assert!(!ns.dir().join("ex.sock").exists());
}

#[test]
fn startup_failure_is_reported_from_the_log() {
    let ns = TestNs::new();
    let err =
        deck_host::start(&ns.0, &spec("bad", "/nonexistent/program", &[]), 80, 24).unwrap_err();
    match err {
        Error::DaemonStartup(msg) => assert!(msg.contains("/nonexistent/program"), "{msg}"),
        other => panic!("expected DaemonStartup, got {other:?}"),
    }
    assert!(!ns.0.has("bad"));
}

#[test]
fn second_daemon_for_the_same_name_is_refused() {
    let ns = TestNs::new();
    deck_host::start(&ns.0, &shell("one"), 80, 24).unwrap();
    assert!(matches!(
        deck_host::start(&ns.0, &shell("one"), 80, 24),
        Err(Error::AlreadyRunning(_))
    ));
}

#[test]
fn protocol_mismatch_is_a_typed_error() {
    let ns = TestNs::new();
    let dir = ns.dir();
    std::fs::create_dir(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    let listener = UnixListener::bind(dir.join("old.sock")).unwrap();
    let server = std::thread::spawn(move || {
        let mut kinds = Vec::new();
        for _ in 0..3 {
            let (mut s, _) = listener.accept().unwrap();
            let mut payload = Vec::new();
            let hello = deck_host::protocol::read_frame(&mut s, &mut payload).unwrap();
            assert_eq!(hello, Some(deck_host::protocol::Kind::Hello));
            let reply = deck_host::protocol::Hello {
                protocol: 99,
                version: "99.0.0".into(),
            };
            deck_host::protocol::write_json(
                &mut s,
                &mut Vec::new(),
                deck_host::protocol::Kind::Hello,
                &reply,
            )
            .unwrap();
            if let Ok(Some(kind)) = deck_host::protocol::read_frame(&mut s, &mut payload) {
                kinds.push(kind);
            }
        }
        kinds
    });
    match ns.0.info("old") {
        Err(Error::ProtocolMismatch {
            ours,
            theirs,
            host_version,
        }) => {
            assert_eq!(
                (ours, theirs, host_version.as_str()),
                (deck_host::PROTOCOL, 99, "99.0.0")
            );
        }
        other => panic!("expected ProtocolMismatch, got {other:?}"),
    }
    assert!(matches!(
        deck_host::attach(&ns.0, "old", params(80, 24)),
        Err(Error::ProtocolMismatch { .. })
    ));
    // Kill speaks to any protocol version.
    let ns_kill = ns.0.clone();
    let killer = std::thread::spawn(move || ns_kill.kill("old"));
    assert_eq!(server.join().unwrap(), [deck_host::protocol::Kind::Kill]);
    killer.join().unwrap().unwrap();
}

#[test]
fn group_accessible_endpoint_dir_is_refused() {
    let ns = TestNs::new();
    let dir = ns.dir();
    std::fs::create_dir(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    let err = deck_host::create_or_attach(&ns.0, &shell("p"), params(80, 24)).err();
    assert!(
        matches!(&err, Some(Error::InsecureDir { path, .. }) if *path == dir),
        "{err:?}"
    );
    assert!(matches!(ns.0.info("p"), Err(Error::InsecureDir { .. })));
    assert!(!ns.0.has("p"));
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
}

#[test]
fn cli_new_ls_dump_kill() {
    let ns = TestNs::new();
    let exe = env!("CARGO_BIN_EXE_omp-deck-host");
    let run = |args: &[&str]| {
        let out = std::process::Command::new(exe).args(args).output().unwrap();
        (
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    };
    let nsa = ns.0.as_str();
    let (ok, stdout, stderr) = run(&[
        "new",
        "--ns",
        nsa,
        "cli",
        "--size",
        "40x10",
        "--",
        "/bin/sh",
        "-c",
        "echo cli-marker; sleep 100",
    ]);
    assert!(ok && stdout.is_empty(), "{stderr}");
    assert_eq!(run(&["ls", "--ns", nsa]).1, "cli\n");
    wait_until("marker on screen", || {
        run(&["dump", "--ns", nsa, "cli"]).1.contains("cli-marker")
    });
    let (ok, _, stderr) = run(&["new", "--ns", nsa, "cli", "--", "/bin/sh"]);
    assert!(!ok && stderr.contains("already running"), "{stderr}");
    let info: serde_json::Value =
        serde_json::from_str(&run(&["info", "--ns", nsa, "cli"]).1).unwrap();
    assert_eq!(info["cols"], 40);
    assert!(run(&["kill", "--ns", nsa, "cli"]).0);
    assert_eq!(run(&["ls", "--ns", nsa]).1, "");
}
