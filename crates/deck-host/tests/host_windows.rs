//! End-to-end tests against real `omp-deck-host` daemons on Windows (ConPTY + named pipes).
//! Every test uses its own namespace `omp-deck-test-<pid>-<n>`; daemons are killed afterwards.
#![cfg(windows)]

mod common;

use std::time::Duration;

use common::{Client, TestNs, env, wait_until};
use deck_host::{Error, HostEvent, SessionSpec};

fn spec(name: &str, program: &str, args: &[&str]) -> SessionSpec {
    SessionSpec {
        name: name.into(),
        program: program.into(),
        args: args.iter().map(|a| (*a).to_owned()).collect(),
        cwd: env().root.clone(),
        env_set: vec![("OMP_DECK_TEST".into(), "1".into())],
        env_remove: Vec::new(),
    }
}

fn cmd(name: &str) -> SessionSpec {
    spec(name, "cmd.exe", &["/d", "/q", "/k", "prompt $G"])
}

fn pid_alive(pid: u32) -> bool {
    let out = std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).contains(&format!("\"{pid}\""))
}

#[test]
fn conpty_round_trip_detach_and_reattach() {
    let ns = TestNs::new();
    let mut c = Client::create(&ns, &cmd("rt"), 80, 24);
    c.type_line("echo marker-%OMP_DECK_TEST%-ok");
    c.wait_output("marker-1-ok");
    c.settle(Duration::from_millis(500));
    let screen = c.screen();
    assert!(screen.contains("marker-1-ok"), "{screen}");

    c.tx.detach().unwrap();
    assert_eq!(c.next(), HostEvent::Closed);
    wait_until("detached state", || !ns.0.info("rt").unwrap().attached);

    let again = Client::attach(&ns, "rt", 80, 24);
    assert!(again.screen().contains("marker-1-ok"), "{}", again.screen());
    let info = ns.0.info("rt").unwrap();
    assert!(info.attached && info.tty.is_none());
    assert_eq!(ns.0.list(), ["rt"]);
}

#[test]
fn resize_reaches_the_console() {
    let ns = TestNs::new();
    let mut c = Client::create(&ns, &cmd("rs"), 80, 24);
    c.tx.resize(101, 33, 9, 18).unwrap();
    c.type_line("mode con");
    c.wait_output("101");
    let info = ns.0.info("rs").unwrap();
    assert_eq!((info.cols, info.rows), (101, 33));
}

#[test]
fn child_exit_code_is_reported() {
    let ns = TestNs::new();
    let mut c = Client::create(
        &ns,
        &spec(
            "ex",
            "cmd.exe",
            &["/d", "/c", "ping -n 2 127.0.0.1 >nul & exit 3"],
        ),
        80,
        24,
    );
    loop {
        match c.next() {
            HostEvent::Exited(code) => {
                assert_eq!(code, Some(3));
                break;
            }
            HostEvent::Output(_) => {}
            other => panic!("unexpected {other:?}"),
        }
    }
    wait_until("endpoint gone", || !ns.0.has("ex"));
}

#[test]
fn kill_terminates_the_job_tree() {
    let ns = TestNs::new();
    let pid_file = env().root.join(format!("gpid-{}", std::process::id()));
    let _ = std::fs::remove_file(&pid_file);
    let script = format!(
        "$p = Start-Process -PassThru -NoNewWindow ping -ArgumentList '-n','1000','127.0.0.1'; \
         Set-Content -Path '{}' -Value $p.Id; Wait-Process -Id $p.Id",
        pid_file.display()
    );
    deck_host::start(
        &ns.0,
        &spec(
            "k",
            "powershell.exe",
            &["-NoProfile", "-NonInteractive", "-Command", &script],
        ),
        80,
        24,
    )
    .unwrap();
    wait_until("grandchild pid", || {
        std::fs::read_to_string(&pid_file).is_ok_and(|s| !s.trim().is_empty())
    });
    let grandchild: u32 = std::fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let info = ns.0.info("k").unwrap();
    assert!(pid_alive(grandchild) && pid_alive(info.child_pid));

    ns.0.kill("k").unwrap();
    assert!(!ns.0.has("k"));
    assert!(ns.0.list().is_empty());
    wait_until("process tree gone", || {
        !pid_alive(grandchild) && !pid_alive(info.child_pid) && !pid_alive(info.daemon_pid)
    });
}

#[test]
fn second_daemon_for_the_same_name_is_refused() {
    let ns = TestNs::new();
    deck_host::start(&ns.0, &cmd("one"), 80, 24).unwrap();
    assert!(matches!(
        deck_host::start(&ns.0, &cmd("one"), 80, 24),
        Err(Error::AlreadyRunning(_))
    ));
}

#[test]
fn startup_failure_is_reported() {
    let ns = TestNs::new();
    let err = deck_host::start(
        &ns.0,
        &spec("bad", r"C:\nonexistent\program.exe", &[]),
        80,
        24,
    )
    .unwrap_err();
    assert!(
        matches!(&err, Error::DaemonStartup(msg) if msg.contains("program.exe")),
        "{err:?}"
    );
    assert!(!ns.0.has("bad"));
}

#[test]
fn queries_are_answered_while_detached() {
    let ns = TestNs::new();
    // ConPTY itself asks for the cursor position at startup and blocks output until answered;
    // a detached session still renders the program's output.
    deck_host::start(
        &ns.0,
        &spec("q", "cmd.exe", &["/d", "/q", "/k", "echo detached-marker"]),
        80,
        24,
    )
    .unwrap();
    wait_until("output rendered", || {
        deck_host::peek(&ns.0, "q", 0)
            .is_ok_and(|(_, snap)| String::from_utf8_lossy(&snap).contains("detached-marker"))
    });
    // Attaching later shows the same screen.
    let c = Client::attach(&ns, "q", 80, 24);
    assert!(c.screen().contains("detached-marker"), "{}", c.screen());
}

#[test]
fn cli_new_ls_dump_kill() {
    let ns = TestNs::new();
    let run = common::run_cli;
    let nsa = ns.0.as_str();
    let (ok, stdout, stderr) = run(&[
        "new",
        "--ns",
        nsa,
        "cli",
        "--",
        "cmd.exe",
        "/d",
        "/q",
        "/k",
        "echo cli-marker",
    ]);
    assert!(ok && stdout.is_empty(), "{stderr}");
    assert_eq!(run(&["ls", "--ns", nsa]).1.trim(), "cli");
    wait_until("marker on screen", || {
        run(&["dump", "--ns", nsa, "cli"]).1.contains("cli-marker")
    });
    assert!(run(&["kill", "--ns", nsa, "cli"]).0);
    assert_eq!(run(&["ls", "--ns", nsa]).1.trim(), "");
}

#[test]
fn child_exiting_at_launch_is_still_served_with_final_screen_and_code() {
    let ns = TestNs::new();
    deck_host::start(
        &ns.0,
        &spec(
            "quick",
            "cmd.exe",
            &["/d", "/c", "echo launch-error& exit 5"],
        ),
        80,
        24,
    )
    .unwrap();
    wait_until("output rendered", || {
        deck_host::peek(&ns.0, "quick", 0)
            .is_ok_and(|(_, snap)| String::from_utf8_lossy(&snap).contains("launch-error"))
    });
    let mut c = Client::attach(&ns, "quick", 80, 24);
    assert!(c.screen().contains("launch-error"), "{}", c.screen());
    assert_eq!(c.next(), HostEvent::Exited(Some(5)));
    wait_until("endpoint gone", || !ns.0.has("quick"));
}
