use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use deck_core::agents::{AgentsScanner, AgentsSnapshot};
use deck_core::fleet::{FleetSession, FleetSnapshot, FleetTarget, FleetWatcher, LAST_REPLY_CHARS};
use serde_json::json;

const WAIT: Duration = Duration::from_secs(20);

fn watcher() -> (FleetWatcher, Arc<AtomicUsize>) {
    let updates = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&updates);
    let w = FleetWatcher::spawn(move || {
        counter.fetch_add(1, Ordering::SeqCst);
    });
    (w, updates)
}

fn wait_for_snapshot(
    w: &FleetWatcher,
    what: &str,
    mut ok: impl FnMut(&FleetSnapshot) -> bool,
) -> Arc<FleetSnapshot> {
    let deadline = Instant::now() + WAIT;
    loop {
        if let Some(s) = w.latest()
            && ok(&s)
        {
            return s;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what}; latest = {:?}",
            w.latest()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn target(session: &Path, cwd: &Path) -> FleetTarget {
    FleetTarget {
        session: session.to_owned(),
        cwd: cwd.to_owned(),
        owner_alive: false,
    }
}

fn find<'a>(s: &'a FleetSnapshot, session: &Path) -> Option<&'a FleetSession> {
    s.sessions
        .iter()
        .find(|f| f.session == session)
        .map(|f| &**f)
}

/// The key order matters: omp writes `type` first and `role` early, and the reader filters on
/// those prefixes.
fn assistant(text: &str, model: &str, cost: f64) -> String {
    let content = json!([{"type": "text", "text": text}]);
    let usage = json!({"input": 100, "output": 20, "cacheRead": 0, "cacheWrite": 0,
                       "cost": {"total": cost}});
    format!(
        "{{\"type\":\"message\",\"message\":{{\"role\":\"assistant\",\"content\":{content},\
         \"provider\":\"p\",\"model\":\"{model}\",\"usage\":{usage},\"stopReason\":\"stop\"}}}}\n"
    )
}

fn session_file(dir: &Path, name: &str, lines: &[String]) -> PathBuf {
    let path = dir.join(name);
    let header = json!({"type": "session", "version": 3, "id": name, "cwd": "/tmp"});
    // Line 0 is omp's title slot: padded to 256 bytes including the newline.
    let slot = json!({"type": "title", "v": 1, "title": name}).to_string();
    let mut body = format!("{slot:<255}\n{header}\n");
    for l in lines {
        body.push_str(l);
    }
    fs::write(&path, body).unwrap();
    path
}

fn append(path: &Path, text: &str) {
    fs::OpenOptions::new()
        .append(true)
        .open(path)
        .unwrap()
        .write_all(text.as_bytes())
        .unwrap();
}

fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let dest = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &dest);
        } else {
            fs::copy(entry.path(), dest).unwrap();
        }
    }
}

#[test]
fn reports_stats_and_agents_in_target_order_and_follows_appended_lines() {
    let dir = tempfile::tempdir().unwrap();
    let a = session_file(
        dir.path(),
        "a.jsonl",
        &[assistant("first answer", "alpha", 0.25)],
    );
    let b = session_file(
        dir.path(),
        "b.jsonl",
        &[
            assistant("other", "beta", 1.0),
            assistant("other again", "beta", 0.5),
        ],
    );
    // A session with subagents: the agents fixture, copied next to the others.
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/agents");
    copy_dir(&fixture, dir.path());
    let c = dir.path().join("sess.jsonl");
    let expected: AgentsSnapshot = AgentsScanner::new(c.clone()).scan(false);
    assert!(!expected.agents.is_empty());

    let (w, updates) = watcher();
    assert!(w.latest().is_none());
    w.set_targets(vec![
        target(&c, dir.path()),
        target(&a, dir.path()),
        target(&b, dir.path()),
    ]);
    let snap = wait_for_snapshot(&w, "all three sessions", |s| s.sessions.len() == 3);
    assert!(updates.load(Ordering::SeqCst) >= 1);

    let order: Vec<&Path> = snap.sessions.iter().map(|s| s.session.as_path()).collect();
    assert_eq!(order, [c.as_path(), a.as_path(), b.as_path()]);
    let fa = find(&snap, &a).unwrap();
    assert_eq!(fa.model.as_deref(), Some("p/alpha"));
    assert_eq!(fa.last_reply.as_deref(), Some("first answer"));
    assert!((fa.cost - 0.25).abs() < 1e-9);
    assert_eq!((fa.input_tokens, fa.output_tokens), (100, 20));
    assert!(fa.agents.is_empty());
    let fb = find(&snap, &b).unwrap();
    assert_eq!(fb.last_reply.as_deref(), Some("other again"));
    assert!((fb.cost - 1.5).abs() < 1e-9);
    assert_eq!(*find(&snap, &c).unwrap().agents, expected.agents);

    let before = Arc::clone(&snap);
    // Appended lines are picked up incrementally.
    append(&a, &assistant("second\n\n  answer", "alpha", 0.5));
    let snap = wait_for_snapshot(&w, "appended reply", |s| {
        find(s, &a).is_some_and(|f| f.last_reply.as_deref() == Some("second answer"))
    });
    let fa = find(&snap, &a).unwrap();
    assert!((fa.cost - 0.75).abs() < 1e-9, "{}", fa.cost);
    assert_eq!(fa.output_tokens, 40);
    assert_eq!(snap.sessions.len(), 3, "other sessions stay");
    // Untouched sessions are shared with the previous snapshot, not copied.
    let arc_of = |s: &FleetSnapshot, p: &Path| {
        Arc::clone(s.sessions.iter().find(|f| f.session == p).unwrap())
    };
    assert!(Arc::ptr_eq(&arc_of(&before, &b), &arc_of(&snap, &b)));
    assert!(Arc::ptr_eq(&arc_of(&before, &c), &arc_of(&snap, &c)));
}

#[test]
fn removed_targets_are_dropped_and_empty_targets_idle() {
    let dir = tempfile::tempdir().unwrap();
    let a = session_file(dir.path(), "a.jsonl", &[assistant("one", "m", 0.1)]);
    let b = session_file(dir.path(), "b.jsonl", &[assistant("two", "m", 0.2)]);
    let (w, updates) = watcher();

    w.set_targets(vec![target(&a, dir.path()), target(&b, dir.path())]);
    wait_for_snapshot(&w, "two sessions", |s| s.sessions.len() == 2);

    w.set_targets(vec![target(&b, dir.path())]);
    let snap = wait_for_snapshot(&w, "only b", |s| s.sessions.len() == 1);
    assert_eq!(snap.sessions[0].session, b);

    // Empty list: no snapshot, and no work even though the file keeps changing.
    w.set_targets(vec![]);
    assert!(w.latest().is_none());
    let quiet_from = updates.load(Ordering::SeqCst);
    append(&b, &assistant("three", "m", 0.3));
    std::thread::sleep(Duration::from_millis(4500));
    assert_eq!(updates.load(Ordering::SeqCst), quiet_from);
    assert!(w.latest().is_none());

    // Coming back reads from scratch: the whole file, including the line appended meanwhile.
    w.set_targets(vec![target(&b, dir.path())]);
    let snap = wait_for_snapshot(&w, "b again", |s| s.sessions.len() == 1);
    assert!((snap.sessions[0].cost - 0.5).abs() < 1e-9);
    assert_eq!(snap.sessions[0].last_reply.as_deref(), Some("three"));
}

#[test]
fn last_reply_is_collapsed_and_capped_on_a_char_boundary() {
    let dir = tempfile::tempdir().unwrap();
    let text = "héllo \t wörld\n".repeat(200);
    let a = session_file(dir.path(), "a.jsonl", &[assistant(&text, "m", 0.0)]);
    let (w, _) = watcher();
    w.set_targets(vec![target(&a, dir.path())]);
    let snap = wait_for_snapshot(&w, "reply", |s| s.sessions.len() == 1);
    let reply = snap.sessions[0].last_reply.as_deref().unwrap();
    assert_eq!(reply.chars().count(), LAST_REPLY_CHARS);
    assert!(reply.starts_with("héllo wörld héllo wörld"));
    assert!(!reply.contains(['\n', '\t']) && !reply.contains("  "));
}

#[test]
fn sessions_sharing_a_cwd_get_the_same_git_counts() {
    if Command::new("git")
        .arg("--version")
        .output()
        .is_err_and(|_| true)
    {
        eprintln!("git not installed; skipping");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    fs::create_dir_all(&repo).unwrap();
    let init = Command::new("git")
        .args(["init", "-q"])
        .current_dir(&repo)
        .output()
        .unwrap();
    assert!(init.status.success());
    fs::write(repo.join("one.txt"), "1").unwrap();
    fs::write(repo.join("two.txt"), "2").unwrap();
    let plain = dir.path().join("plain");
    fs::create_dir_all(&plain).unwrap();

    let a = session_file(dir.path(), "a.jsonl", &[assistant("a", "m", 0.0)]);
    let b = session_file(dir.path(), "b.jsonl", &[assistant("b", "m", 0.0)]);
    let c = session_file(dir.path(), "c.jsonl", &[assistant("c", "m", 0.0)]);
    let (w, _) = watcher();
    w.set_targets(vec![
        target(&a, &repo),
        target(&b, &repo),
        target(&c, &plain),
    ]);
    let snap = wait_for_snapshot(&w, "git counts", |s| {
        s.sessions.len() == 3 && [&a, &b].iter().all(|p| find(s, p).unwrap().git.is_some())
    });
    let ga = find(&snap, &a).unwrap().git.clone().unwrap();
    assert_eq!(ga.untracked, 2);
    assert_eq!(find(&snap, &b).unwrap().git.as_ref(), Some(&ga));
    assert_eq!(find(&snap, &c).unwrap().git, None, "not a repository");
}
