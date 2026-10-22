use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant, SystemTime};

use deck_core::tmux::{
    Tmux, config_text, direct_launch, effective_cwd, new_session_name, session_name_for,
    terminal_env,
};

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
    let fallback = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
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
    ] {
        assert!(env.lines().any(|l| l == line), "{line} missing: {env}");
    }
    let out_file = dir.path().join("pane-env");
    let script = format!(
        "echo \"$PI_FORCE_IMAGE_PROTOCOL $PI_KITTY_PLACEHOLDERS $PI_FORCE_HYPERLINKS\" > '{}'",
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
    assert_eq!(fs::read_to_string(&out_file).unwrap(), "kitty 1 1\n");
}
