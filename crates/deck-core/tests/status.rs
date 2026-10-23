use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime};

use deck_core::mux::Mux;
use deck_core::status::{SessionStatus, StatusTarget, StatusWatcher, breadcrumb_path};
use deck_core::tmux::Tmux;

const SESSION: &str = include_str!("fixtures/status/session.jsonl");

/// Kills the test server on drop (also on panic) and removes its socket file.
struct Server {
    tmux: Tmux,
}

impl Drop for Server {
    fn drop(&mut self) {
        let path = Command::new("tmux")
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
            .map(|o| PathBuf::from(String::from_utf8_lossy(&o.stdout).trim()));
        let _ = Command::new("tmux")
            .args(["-L", self.tmux.socket(), "kill-server"])
            .output();
        if let Some(p) = path.filter(|p| !p.as_os_str().is_empty()) {
            let _ = fs::remove_file(p);
        }
    }
}

fn wait_for<T>(what: &str, mut probe: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(v) = probe() {
            return v;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(30));
    }
}

fn watcher_with_counter(agent_dir: &Path) -> (StatusWatcher, Arc<AtomicUsize>) {
    let updates = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&updates);
    let w = StatusWatcher::spawn(agent_dir.to_path_buf(), move || {
        counter.fetch_add(1, Ordering::SeqCst);
    });
    (w, updates)
}

fn assistant_line(input: u64) -> String {
    format!(
        r#"{{"type":"message","id":"n","parentId":null,"timestamp":"t","message":{{"role":"assistant","content":[{{"type":"text","text":"more"}}],"provider":"commandcode","model":"deepseek/deepseek-v4.1-flash","usage":{{"input":{input},"output":1,"cacheRead":0,"cacheWrite":0,"totalTokens":{},"cost":{{"total":0.5}}}},"stopReason":"stop"}}}}
"#,
        input + 1
    )
}

#[test]
fn watcher_follows_appends_publishes_only_changes_and_stops_promptly() {
    let dir = tempfile::tempdir().unwrap();
    let session = dir.path().join("s.jsonl");
    fs::write(&session, SESSION).unwrap();
    let (watcher, updates) = watcher_with_counter(dir.path());
    assert!(watcher.latest().is_none());

    let target = StatusTarget {
        mux: None,
        session: Some(session.clone()),
        cwd: dir.path().to_path_buf(),
    };
    watcher.set_target(Some(target.clone()));
    let first = wait_for("first snapshot", || watcher.latest());
    assert_eq!(first.session.as_deref(), Some(session.as_path()));
    assert!(!first.fresh);
    assert_eq!(first.cwd, dir.path());
    assert_eq!(first.stats.input_tokens, 17268);
    assert_eq!(first.git, None, "tempdir is no repository");
    let n = updates.load(Ordering::SeqCst);
    assert!(n >= 1);

    // Nothing changes → nothing is published (same Arc, no callback) across several ticks.
    std::thread::sleep(Duration::from_millis(1500));
    assert!(Arc::ptr_eq(&first, &watcher.latest().unwrap()));
    assert_eq!(updates.load(Ordering::SeqCst), n);
    // Setting an equal target keeps the snapshot.
    watcher.set_target(Some(target));
    assert!(Arc::ptr_eq(&first, &watcher.latest().unwrap()));

    fs::OpenOptions::new()
        .append(true)
        .open(&session)
        .unwrap()
        .write_all(assistant_line(1000).as_bytes())
        .unwrap();
    let next: Arc<SessionStatus> = wait_for("appended line", || {
        watcher.latest().filter(|s| s.stats.input_tokens == 18268)
    });
    assert_eq!(next.stats.context_tokens, Some(1000));
    assert_eq!(updates.load(Ordering::SeqCst), n + 1);

    // A new target drops the old snapshot immediately.
    watcher.set_target(None);
    assert!(watcher.latest().is_none());

    let t = Instant::now();
    drop(watcher);
    assert!(t.elapsed() < Duration::from_secs(2), "drop joins promptly");
}

fn tmux_available() -> bool {
    Command::new("tmux")
        .arg("-V")
        .output()
        .is_ok_and(|o| o.status.success())
}

#[test]
fn breadcrumb_overrides_target_session_only_when_not_stale() {
    if !tmux_available() {
        eprintln!("tmux not installed; skipping");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let agent = dir.path().join("agent");
    fs::create_dir_all(agent.join("terminal-sessions")).unwrap();
    let known = dir.path().join("known.jsonl"); // the tab's own idea
    let crumb_target = dir.path().join("moved.jsonl"); // what omp reports after /new
    fs::write(&known, SESSION).unwrap();
    // moved.jsonl: header + a single assistant reply (input 42).
    fs::write(
        &crumb_target,
        format!(
            "{}\n{}\n{}",
            SESSION.lines().next().unwrap(),
            SESSION.lines().nth(1).unwrap(),
            assistant_line(42).trim_end()
        ) + "\n",
    )
    .unwrap();
    let project = dir.path().join("proj");
    fs::create_dir(&project).unwrap();

    let socket = format!("omp-deck-test-status-{}", std::process::id());
    let tmux = Tmux::with_socket("tmux", socket, dir.path().join("tmux.conf"));
    let _server = Server { tmux: tmux.clone() };
    tmux.ensure_config().unwrap();
    let before = SystemTime::now();
    let started = Command::new("tmux")
        .args(["-L", tmux.socket(), "-f"])
        .arg(dir.path().join("tmux.conf"))
        .args(["new-session", "-d", "-s", "omp-st", "--", "sleep", "60"])
        .env_remove("TMUX")
        .output()
        .unwrap();
    assert!(
        started.status.success(),
        "{}",
        String::from_utf8_lossy(&started.stderr)
    );

    assert_eq!(tmux.pane("omp-missing"), None);
    let pane = tmux.pane("omp-st").unwrap();
    assert!(
        pane.breadcrumb_key.starts_with("pts-"),
        "{}",
        pane.breadcrumb_key
    );
    let pane_start = pane.started;
    let skew = |a: SystemTime, b: SystemTime| {
        a.duration_since(b)
            .unwrap_or_else(|e| e.duration())
            .as_secs_f64()
    };
    assert!(skew(pane_start, before) < 5.0, "pane start ≈ now");

    let crumb = breadcrumb_path(&agent, &pane.breadcrumb_key);
    let write_crumb = |session: &Path, fresh: bool| {
        let fresh = if fresh { "fresh\n" } else { "" };
        fs::write(
            &crumb,
            format!(
                "{}\n{}\n{fresh}cwdstat 1 2\n",
                project.display(),
                session.display()
            ),
        )
        .unwrap();
    };

    let (watcher, _updates) = watcher_with_counter(&agent);
    watcher.set_target(Some(StatusTarget {
        mux: Some((Mux::Tmux(tmux.clone()), "omp-st".into())),
        session: Some(known.clone()),
        cwd: dir.path().to_path_buf(),
    }));

    // No breadcrumb yet → the tab's own session.
    let s = wait_for("fallback snapshot", || watcher.latest());
    assert_eq!(s.session.as_deref(), Some(known.as_path()));
    assert_eq!(s.cwd, dir.path());

    // omp rewrote the breadcrumb (e.g. /new): it wins, with its cwd and fresh flag.
    write_crumb(&crumb_target, true);
    watcher.refresh_now();
    let s = wait_for("breadcrumb snapshot", || {
        watcher
            .latest()
            .filter(|s| s.session.as_deref() == Some(crumb_target.as_path()))
    });
    assert!(s.fresh);
    assert_eq!(s.cwd, project);
    assert_eq!(s.stats.input_tokens, 42);

    // A breadcrumb older than the pane's process belongs to a previous pty owner: ignored.
    let f = fs::OpenOptions::new().write(true).open(&crumb).unwrap();
    f.set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000))
        .unwrap();
    drop(f);
    watcher.refresh_now();
    let s = wait_for("stale breadcrumb ignored", || {
        watcher
            .latest()
            .filter(|s| s.session.as_deref() == Some(known.as_path()))
    });
    assert!(!s.fresh);
    assert_eq!(s.stats.input_tokens, 17268);

    // …except when it names the session the tab already runs (omp leaves an unchanged
    // breadcrumb alone on `--resume`): then its cwd is used.
    write_crumb(&known, false);
    let f = fs::OpenOptions::new().write(true).open(&crumb).unwrap();
    f.set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000))
        .unwrap();
    drop(f);
    watcher.refresh_now();
    let s = wait_for("stale-but-agreeing breadcrumb", || {
        watcher.latest().filter(|s| s.cwd == project)
    });
    assert_eq!(s.session.as_deref(), Some(known.as_path()));
}
