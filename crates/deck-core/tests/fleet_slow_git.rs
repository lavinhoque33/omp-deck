//! A slow `git` must neither delay session updates nor block dropping the watcher. Own test
//! binary: it puts a fake `git` first on `PATH` for the whole process.
#![cfg(unix)]

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use deck_core::fleet::{FleetTarget, FleetWatcher};

fn install_slow_git(dir: &Path) {
    let script = dir.join("git");
    fs::write(&script, "#!/bin/sh\nsleep 1.5\necho '# branch.head main'\n").unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    let path = std::env::var_os("PATH").unwrap_or_default();
    let mut paths = vec![dir.to_owned()];
    paths.extend(std::env::split_paths(&path));
    // SAFETY: first statement of the only test in this binary; no other thread exists yet.
    unsafe { std::env::set_var("PATH", std::env::join_paths(paths).unwrap()) };
}

fn session(dir: &Path, name: &str, text: &str) -> PathBuf {
    let path = dir.join(name);
    let slot = r#"{"type":"title","v":1,"title":"t"}"#;
    fs::write(&path, format!("{slot:<255}\n{}", assistant(text))).unwrap();
    path
}

fn assistant(text: &str) -> String {
    format!(
        "{{\"type\":\"message\",\"message\":{{\"role\":\"assistant\",\"content\":[{{\"type\":\"text\",\"text\":\"{text}\"}}],\"stopReason\":\"stop\"}}}}\n"
    )
}

fn reply_of(w: &FleetWatcher, session: &Path) -> Option<String> {
    w.latest()?
        .sessions
        .iter()
        .find(|s| s.session == session)?
        .last_reply
        .clone()
}

#[test]
fn slow_git_runs_never_delay_session_updates_and_drop_returns_promptly() {
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("bin");
    fs::create_dir_all(&bin).unwrap();
    install_slow_git(&bin);

    // Four sessions in four cwds: the fake git needs 4 × 1.5 s per round, back to back.
    let mut targets = Vec::new();
    for i in 0..4 {
        let cwd = dir.path().join(format!("cwd{i}"));
        fs::create_dir_all(&cwd).unwrap();
        targets.push(FleetTarget {
            session: session(dir.path(), &format!("s{i}.jsonl"), "before"),
            cwd,
            owner_alive: false,
        });
    }
    let watched = targets[3].session.clone();
    let w = FleetWatcher::spawn(|| {});
    w.set_targets(targets);

    let deadline = Instant::now() + Duration::from_secs(20);
    while w.latest().is_none_or(|s| s.sessions.len() < 4) {
        assert!(Instant::now() < deadline, "first pass");
        std::thread::sleep(Duration::from_millis(50));
    }
    // Git is now busy for several seconds; an appended line must still show up within one idle
    // cycle (3 s) plus slack.
    fs::OpenOptions::new()
        .append(true)
        .open(&watched)
        .unwrap()
        .write_all(assistant("after").as_bytes())
        .unwrap();
    let appended = Instant::now();
    while reply_of(&w, &watched).as_deref() != Some("after") {
        assert!(
            appended.elapsed() < Duration::from_millis(4500),
            "session update starved behind git: {:?}",
            appended.elapsed()
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    // Git results do arrive, and dropping while one is running does not wait for it.
    let deadline = Instant::now() + Duration::from_secs(20);
    while w
        .latest()
        .is_none_or(|s| s.sessions.iter().any(|s| s.git.is_none()))
    {
        assert!(Instant::now() < deadline, "git results");
        std::thread::sleep(Duration::from_millis(50));
    }
    std::thread::sleep(Duration::from_millis(1700)); // inside the next round of git runs
    let started = Instant::now();
    drop(w);
    assert!(
        started.elapsed() < Duration::from_millis(1000),
        "drop blocked for {:?}",
        started.elapsed()
    );
}
