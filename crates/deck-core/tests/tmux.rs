use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant, SystemTime};

use deck_core::mux::{
    direct_launch, effective_cwd, new_session_name, session_name_for, terminal_env,
};
use deck_core::tmux::{Tmux, config_text};

/// Kills the test server on drop (also on panic) and removes its socket file, which tmux
/// leaves behind after `kill-server`.
struct Server {
    tmux: Tmux,
}

impl Server {
    /// Exact socket path: asked of the live server, else `${TMUX_TMPDIR:-/tmp}/tmux-<uid>/<name>`.
    fn socket_path(&self) -> Option<PathBuf> {
        let live = Command::new("tmux")
            .args([
                "-L",
                self.tmux.socket(),
                "display-message",
                "-p",
                "#{socket_path}",
            ])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .filter(|s| !s.is_empty());
        if let Some(p) = live {
            return Some(PathBuf::from(p));
        }
        let uid = Command::new("id").arg("-u").output().ok()?;
        let uid = String::from_utf8_lossy(&uid.stdout).trim().to_string();
        let base =
            std::env::var_os("TMUX_TMPDIR").map_or_else(|| PathBuf::from("/tmp"), PathBuf::from);
        Some(base.join(format!("tmux-{uid}")).join(self.tmux.socket()))
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let path = self.socket_path();
        let _ = Command::new("tmux")
            .args(["-L", self.tmux.socket(), "kill-server"])
            .output();
        if let Some(p) = path {
            let _ = fs::remove_file(p);
        }
    }
}

fn tmux_available() -> bool {
    Command::new("tmux")
        .arg("-V")
        .output()
        .is_ok_and(|o| o.status.success())
}

#[test]
fn session_name_sanitization() {
    assert_eq!(session_name_for("01a15c89-ab_C9"), "omp-01a15c89-ab_C9");
    assert_eq!(session_name_for("we ird:id.x/é"), "omp-we_ird_id_x__");
    assert_eq!(
        new_session_name(SystemTime::UNIX_EPOCH + Duration::from_millis(1234)),
        "omp-new-1234"
    );
}

#[test]
fn effective_cwd_falls_back_to_home_for_missing_dirs() {
    let d = tempfile::tempdir().unwrap();
    assert_eq!(effective_cwd(d.path()), d.path());
    let fallback = deck_core::platform::home_or_root();
    assert_eq!(effective_cwd(&d.path().join("gone")), fallback);
    let file = d.path().join("f");
    fs::write(&file, b"").unwrap();
    assert_eq!(effective_cwd(&file), fallback);
}

#[test]
fn launch_specs_use_dedicated_socket_and_clean_env() {
    let d = tempfile::tempdir().unwrap();
    let t = Tmux::with_socket("/usr/bin/tmux", "sock-x", d.path().join("t.conf"));
    let argv = vec![
        "omp".to_string(),
        "--resume".to_string(),
        "/a b/s.jsonl".to_string(),
    ];
    let spec = t.attach_or_create("omp-1", d.path(), &argv);
    assert_eq!(spec.program, "env");
    let conf = d.path().join("t.conf").to_string_lossy().into_owned();
    let cwd = d.path().to_string_lossy().into_owned();
    let mut expected: Vec<String> = [
        "-u",
        "TMUX",
        "-u",
        "WINDOWID",
        "-u",
        "ALACRITTY_WINDOW_ID",
        "/usr/bin/tmux",
        "-L",
        "sock-x",
        "-f",
        &conf,
        "new-session",
        "-A",
        "-s",
        "omp-1",
        "-c",
        &cwd,
        "--",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    expected.extend(argv.clone());
    assert_eq!(spec.args, expected);
    assert_eq!(spec.env, terminal_env());

    let direct = direct_launch(&d.path().join("missing"), &argv);
    assert_eq!(direct.program, "env");
    assert_eq!(
        direct.args,
        [
            "-u",
            "WINDOWID",
            "-u",
            "ALACRITTY_WINDOW_ID",
            "omp",
            "--resume",
            "/a b/s.jsonl"
        ]
    );
    assert_eq!(direct.cwd, effective_cwd(&d.path().join("missing")));
}

fn wait_for(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn tmux_roundtrip_on_test_socket() {
    if !tmux_available() {
        eprintln!("tmux not installed; skipping tmux_roundtrip_on_test_socket");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("sessions");
    let proj = root.join("-proj");
    fs::create_dir_all(proj.join("artifacts")).unwrap();
    let session_file = proj.join("2026-01-01T00-00-00-000Z_abc.jsonl");
    let nested_file = proj.join("artifacts/Agent.jsonl");
    let other_file = proj.join("notes.txt");

    let socket = format!("omp-deck-test-{}", std::process::id());
    let tmux = Tmux::with_socket("tmux", socket, dir.path().join("cfg/tmux.conf"));
    let server = Server { tmux: tmux.clone() };

    assert!(
        tmux.alive_sessions().is_empty(),
        "no server yet → empty, not an error"
    );
    assert!(tmux.session_files(&root).is_empty());
    assert!(!tmux.has_session("omp-t1"));
    assert!(tmux.kill("omp-t1").is_err());

    let conf = tmux.ensure_config().unwrap();
    assert_eq!(fs::read_to_string(&conf).unwrap(), config_text());

    // The pane holds a top-level session file, a nested artifact file, and a non-jsonl file
    // open from a grandchild of the pane process; only the first must be reported.
    let script = format!(
        "exec 3>>'{}' 4>>'{}' 5>>'{}'; sh -c 'sleep 30'",
        session_file.display(),
        nested_file.display(),
        other_file.display()
    );
    let argv = vec!["sh".to_string(), "-c".to_string(), script];
    let spec = tmux.attach_or_create("omp-t1", dir.path(), &argv);
    let mut args = spec.args.clone();
    let at = args.iter().position(|a| a == "new-session").unwrap() + 1;
    args.insert(at, "-d".to_string());
    let out = Command::new(&spec.program).args(&args).output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    assert!(tmux.alive_sessions().contains("omp-t1"));
    assert!(tmux.has_session("omp-t1"));
    assert!(!tmux.has_session("omp-t"), "exact-name match, not prefix");

    // Server is now running: re-applying the config goes through source-file.
    tmux.ensure_config().unwrap();
    let status_off = Command::new("tmux")
        .args(["-L", tmux.socket(), "show-options", "-gv", "status"])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&status_off.stdout).trim(), "off");

    let keys = Command::new("tmux")
        .args(["-L", tmux.socket(), "list-keys", "-T", "prefix"])
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&keys.stdout).trim().is_empty(),
        "prefix table must be empty after re-sourcing"
    );

    wait_for("session_files to see the open file", || {
        tmux.session_files(&root)
            .get("omp-t1")
            .is_some_and(|f| !f.is_empty())
    });
    let files = tmux.session_files(&root);
    assert_eq!(
        files["omp-t1"],
        vec![fs::canonicalize(&session_file).unwrap()]
    );
    // A different root matches nothing.
    assert!(tmux.session_files(Path::new("/nonexistent-root"))["omp-t1"].is_empty());

    tmux.kill("omp-t1").unwrap();
    assert!(!tmux.has_session("omp-t1"));
    assert!(tmux.alive_sessions().is_empty());
    drop(server);
    assert!(tmux.alive_sessions().is_empty(), "dead server → empty set");
}

#[test]
fn generated_config_sources_cleanly_twice() {
    if !tmux_available() {
        eprintln!("tmux not installed; skipping generated_config_sources_cleanly_twice");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let socket = format!("omp-deck-test-{}-conf", std::process::id());
    let tmux = Tmux::with_socket("tmux", socket, dir.path().join("tmux.conf"));
    let _server = Server { tmux: tmux.clone() };
    let conf = tmux.ensure_config().unwrap();
    let start = Command::new("tmux")
        .args(["-L", tmux.socket(), "-f"])
        .arg(&conf)
        .args(["new-session", "-d", "-s", "c", "--", "sleep", "30"])
        .output()
        .unwrap();
    assert!(
        start.status.success(),
        "{}",
        String::from_utf8_lossy(&start.stderr)
    );
    for round in 1..=2 {
        let out = Command::new("tmux")
            .args(["-L", tmux.socket(), "source-file"])
            .arg(&conf)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "source-file #{round}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let keys = Command::new("tmux")
        .args(["-L", tmux.socket(), "list-keys", "-T", "copy-mode"])
        .output()
        .unwrap();
    let keys = String::from_utf8_lossy(&keys.stdout);
    assert!(
        keys.contains("Any"),
        "copy-mode table lacks the Any binding: {keys}"
    );

    // omp's image/hyperlink overrides reach the global environment and new panes.
    let env = Command::new("tmux")
        .args(["-L", tmux.socket(), "show-environment", "-g"])
        .output()
        .unwrap();
    let env = String::from_utf8_lossy(&env.stdout);
    for line in [
        "PI_FORCE_IMAGE_PROTOCOL=kitty",
        "PI_KITTY_PLACEHOLDERS=1",
        "PI_FORCE_HYPERLINKS=1",
        "PI_NOTIFICATIONS=off",
    ] {
        assert!(env.lines().any(|l| l == line), "{line} missing: {env}");
    }
    let out_file = dir.path().join("pane-env");
    let script = format!(
        "echo \"$PI_FORCE_IMAGE_PROTOCOL $PI_KITTY_PLACEHOLDERS $PI_FORCE_HYPERLINKS $PI_NOTIFICATIONS\" > '{}'",
        out_file.display()
    );
    let pane = Command::new("tmux")
        .args([
            "-L",
            tmux.socket(),
            "new-session",
            "-d",
            "-s",
            "e",
            "--",
            "sh",
            "-c",
        ])
        .arg(&script)
        .output()
        .unwrap();
    assert!(pane.status.success());
    wait_for("the pane to write its env", || {
        fs::read_to_string(&out_file).is_ok_and(|s| s.ends_with('\n'))
    });
    assert_eq!(fs::read_to_string(&out_file).unwrap(), "kitty 1 1 off\n");
}

/// tmux (3.7c, `tty-keys.c:1014`) dereferences `c->session->curw` when a client whose session
/// was destroyed still has focus-in (`ESC [ I`) bytes in its tty buffer: the whole server
/// segfaults and takes every other session down. `Tmux::kill` must detach the clients first.
#[cfg(unix)]
#[test]
fn killing_a_session_whose_client_reports_focus_does_not_crash_the_server() {
    use std::io::Write;
    use std::os::fd::{FromRawFd, OwnedFd};
    use std::os::unix::process::CommandExt;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    if !tmux_available() {
        eprintln!("tmux not installed; skipping");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let socket = format!("omp-deck-test-{}-focus", std::process::id());
    let tmux = Tmux::with_socket("tmux", socket, dir.path().join("cfg/tmux.conf"));
    let _server = Server { tmux: tmux.clone() };
    let conf = tmux.ensure_config().unwrap();

    for name in ["omp-a", "omp-b"] {
        let argv = vec!["sleep".to_string(), "60".to_string()];
        let spec = tmux.attach_or_create(name, dir.path(), &argv);
        let mut args = spec.args.clone();
        let at = args.iter().position(|a| a == "new-session").unwrap() + 1;
        args.insert(at, "-d".to_string());
        let out = Command::new(&spec.program).args(&args).output().unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    // A real tmux client attached to A through a PTY, so the server reads that tty.
    let (mut master, mut slave) = (0, 0);
    // SAFETY: valid out-pointers; null name/termios/winsize are allowed.
    let rc = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    assert_eq!(rc, 0);
    // SAFETY: both fds are fresh and owned here.
    let (master, slave) = unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
    let mut client = Command::new("tmux");
    client
        .args(["-L", tmux.socket(), "-f"])
        .arg(&conf)
        .args(["attach-session", "-t", "=omp-a"])
        .env("TERM", "xterm-256color")
        .env_remove("TMUX")
        .stdin(slave.try_clone().unwrap())
        .stdout(slave.try_clone().unwrap())
        .stderr(slave);
    // SAFETY: only async-signal-safe calls (setsid, ioctl) between fork and exec.
    unsafe {
        client.pre_exec(|| {
            if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut client = client.spawn().unwrap();

    let attached = || {
        Command::new("tmux")
            .args(["-L", tmux.socket(), "list-clients", "-t", "=omp-a"])
            .output()
            .is_ok_and(|o| o.status.success() && !o.stdout.is_empty())
    };
    let end = Instant::now() + Duration::from_secs(5);
    while !attached() && Instant::now() < end {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(attached(), "the client never attached");

    // The app's focus reports while its Stop dialog is open: a steady stream of focus in/out.
    let stop = Arc::new(AtomicBool::new(false));
    let writer = {
        let stop = Arc::clone(&stop);
        let mut tty = fs::File::from(master);
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                if tty.write_all(b"\x1b[I\x1b[O").is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_micros(200));
            }
        })
    };

    let result = tmux.kill("omp-a");
    std::thread::sleep(Duration::from_millis(300));
    stop.store(true, Ordering::Relaxed);
    let _ = writer.join();
    let _ = client.kill();
    let _ = client.wait();

    result.unwrap();
    let alive = tmux.alive_sessions();
    assert!(
        alive.contains("omp-b"),
        "the server died with the killed session: alive = {alive:?}"
    );
    assert!(!alive.contains("omp-a"), "{alive:?}");
}

/// The tmux server inherits its first client's process cwd. When that client was launched for a
/// worktree that is later removed, the server must not be stranded in the deleted directory
/// (new panes would die at once with `process.cwd failed`).
#[cfg(target_os = "linux")]
#[test]
fn server_survives_deletion_of_the_first_sessions_folder() {
    if !tmux_available() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let tmux = Tmux::with_socket(
        "tmux",
        format!("omp-deck-test-{}-gone", std::process::id()),
        dir.path().join("t.conf"),
    );
    let _server = Server { tmux: tmux.clone() };
    tmux.ensure_config().unwrap();

    let doomed = dir.path().join("doomed");
    let other = dir.path().join("other");
    fs::create_dir(&doomed).unwrap();
    fs::create_dir(&other).unwrap();
    let start = |name: &str, cwd: &Path| {
        let argv = vec!["sleep".to_string(), "30".to_string()];
        let spec = tmux.attach_or_create(name, cwd, &argv);
        let mut args = spec.args.clone();
        let at = args.iter().position(|a| a == "new-session").unwrap() + 1;
        args.insert(at, "-d".to_string());
        Command::new(&spec.program)
            .args(&args)
            .current_dir(&spec.cwd)
            .output()
            .unwrap()
    };
    assert!(start("omp-first", &doomed).status.success());
    fs::remove_dir_all(&doomed).unwrap();

    let out = start("omp-second", &other);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let pid = Command::new("tmux")
        .args(["-L", tmux.socket(), "display-message", "-p", "#{pid}"])
        .output()
        .unwrap();
    let pid = String::from_utf8_lossy(&pid.stdout).trim().to_owned();
    assert!(
        fs::read_link(format!("/proc/{pid}/cwd")).is_ok_and(|p| p.exists()),
        "server cwd must still exist"
    );
    assert!(tmux.alive_sessions().contains("omp-second"));
}

/// `Tmux::pane_titles` reads the OSC 2 titles of every session's active pane in one pass; titles
/// with spaces and non-ASCII text arrive intact (tmux strips tabs itself), and a session whose pane set none is
/// absent from the map.
#[test]
fn pane_titles_report_each_sessions_active_pane_title() {
    if !tmux_available() {
        eprintln!("tmux not installed; skipping pane_titles test");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let socket = format!("omp-deck-test-{}-titles", std::process::id());
    let tmux = Tmux::with_socket("tmux", socket, dir.path().join("cfg/tmux.conf"));
    let _server = Server { tmux: tmux.clone() };
    assert!(tmux.pane_titles().is_empty(), "no server → empty");
    tmux.ensure_config().unwrap();

    let start = |name: &str, script: &str| {
        let argv = vec!["sh".to_string(), "-c".to_string(), script.to_string()];
        let spec = tmux.attach_or_create(name, dir.path(), &argv);
        let mut args = spec.args.clone();
        let at = args.iter().position(|a| a == "new-session").unwrap() + 1;
        args.insert(at, "-d".to_string());
        let out = Command::new(&spec.program).args(&args).output().unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    start(
        "omp-ta",
        "printf '\\033]2;π Working: my task\\twith tab\\007'; sleep 30",
    );
    start("omp-tb", "printf '\\033]2;second\\007'; sleep 30");
    start("omp-tc", "sleep 30");

    // A pane's title is the hostname until its program's OSC 2 arrives.
    wait_for("both titles", || {
        let t = tmux.pane_titles();
        t.get("omp-tb").is_some_and(|t| t == "second")
            && t.get("omp-ta").is_some_and(|t| t.starts_with('π'))
    });
    let titles = tmux.pane_titles();
    // tmux itself drops the tab from a title (control characters are stripped).
    assert_eq!(titles["omp-ta"], "π Working: my taskwith tab");
    assert_eq!(titles["omp-tb"], "second");
    assert_ne!(titles.get("omp-tc").map(String::as_str), Some("second"));
}
