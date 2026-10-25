//! `Omp::delete_sessions` against the real `omp` in a throwaway agent dir: removes exactly the
//! sessions' own files, refuses a session held by a live omp (without blocking the rest of the
//! batch), rejects non-session files, streams one result per target.
//! Never contacts a real provider. Skips (with a message) when `omp` or `tmux` is not installed.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use deck_core::omp::{DeleteError, DeleteTarget, Omp};

const HEADER: &str = r#"{"type":"session","version":3,"id":"ID","timestamp":"2026-01-01T00:00:00.000Z","cwd":"/tmp"}"#;
const USER: &str = r#"{"type":"message","id":"m1","parentId":null,"timestamp":"2026-01-01T00:00:01.000Z","message":{"role":"user","content":"hi","timestamp":1}}"#;
const ASSISTANT: &str = r#"{"type":"message","id":"m2","parentId":"m1","timestamp":"2026-01-01T00:00:02.000Z","message":{"role":"assistant","content":[{"type":"text","text":"hello"}],"api":"anthropic-messages","provider":"anthropic","model":"claude-sonnet-4-5","usage":{"input":1,"output":1,"cacheRead":0,"cacheWrite":0,"totalTokens":2,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}},"stopReason":"stop","timestamp":2}"#;

struct Sandbox {
    _tmp: tempfile::TempDir,
    agent: PathBuf,
    config: PathBuf,
    dir: PathBuf,
    omp: Omp,
}

fn sandbox() -> Option<Sandbox> {
    let Some(omp) = Omp::detect() else {
        eprintln!("omp not found on PATH; skipping delete_sessions integration test");
        return None;
    };
    let tmp = tempfile::tempdir().unwrap();
    let agent = tmp.path().join("agent");
    let config = tmp.path().join("config");
    let dir = agent.join("sessions/--tmp-x--");
    fs::create_dir_all(&dir).unwrap();
    fs::create_dir_all(&config).unwrap();
    let omp = omp
        .with_env("PI_CODING_AGENT_DIR", &agent)
        .with_env("PI_CONFIG_DIR", &config);
    Some(Sandbox {
        _tmp: tmp,
        agent,
        config,
        dir,
        omp,
    })
}

impl Sandbox {
    fn session(&self, stem: &str, id: &str) -> PathBuf {
        let p = self.dir.join(format!("{stem}.jsonl"));
        let header = HEADER.replace("ID", id);
        // omp's own files start with a padded title line; the header is then line 2.
        let title = r#"{"type":"title","v":1,"title":"t","updatedAt":"2026-01-01T00:00:00.000Z","pad":"   "}"#;
        fs::write(&p, format!("{title}\n{header}\n{USER}\n{ASSISTANT}\n")).unwrap();
        p
    }

    /// `<stem>/artifacts/a.txt`, `<stem>/sub.jsonl` and `<stem>.jsonl.1.bak`.
    fn extras(&self, stem: &str) -> Vec<PathBuf> {
        let d = self.dir.join(stem);
        fs::create_dir_all(d.join("artifacts")).unwrap();
        let a = d.join("artifacts/a.txt");
        let sub = d.join("sub.jsonl");
        let bak = self.dir.join(format!("{stem}.jsonl.1.bak"));
        for p in [&a, &sub, &bak] {
            fs::write(p, "x").unwrap();
        }
        vec![d, a, sub, bak]
    }

    fn listing(&self) -> Vec<String> {
        let mut out = Vec::new();
        fn walk(base: &Path, p: &Path, out: &mut Vec<String>) {
            for e in fs::read_dir(p).unwrap().flatten() {
                out.push(e.path().strip_prefix(base).unwrap().display().to_string());
                if e.path().is_dir() {
                    walk(base, &e.path(), out);
                }
            }
        }
        walk(&self.dir, &self.dir, &mut out);
        out.sort();
        out
    }
}

/// The id the file's header holds now (what a UI captures at selection time); `"?"` when none.
fn id_of(p: &Path) -> String {
    let text = fs::read_to_string(p).unwrap_or_default();
    text.lines()
        .take(3)
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find(|v| v["type"] == "session")
        .and_then(|v| v["id"].as_str().map(str::to_owned))
        .unwrap_or_else(|| "?".into())
}

fn target_of(p: &Path) -> DeleteTarget {
    DeleteTarget {
        path: p.to_path_buf(),
        id: id_of(p),
    }
}

fn run(omp: &Omp, targets: &[DeleteTarget]) -> Vec<(PathBuf, Result<(), DeleteError>)> {
    let mut out = Vec::new();
    omp.delete_sessions(targets, |p, r| out.push((p.to_path_buf(), r)));
    out
}

/// Runs one batch (ids captured from the files as they are now) and returns the results in
/// arrival order.
fn batch(omp: &Omp, files: &[&Path]) -> Vec<(PathBuf, Result<(), DeleteError>)> {
    let targets: Vec<DeleteTarget> = files.iter().map(|p| target_of(p)).collect();
    run(omp, &targets)
}

fn result_of<'a>(
    all: &'a [(PathBuf, Result<(), DeleteError>)],
    p: &Path,
) -> &'a Result<(), DeleteError> {
    let hits: Vec<_> = all.iter().filter(|(q, _)| q == p).collect();
    assert_eq!(hits.len(), 1, "exactly one result for {}", p.display());
    &hits[0].1
}

#[test]
fn deletes_exactly_the_sessions_and_their_own_files_in_one_batch() {
    let Some(sb) = sandbox() else { return };
    let a = sb.session("2026-01-01T00-00-00-000Z_aaaa", "aaaa1111bbbb2222");
    let owned_a = sb.extras("2026-01-01T00-00-00-000Z_aaaa");
    let b = sb.session("2026-01-03T00-00-00-000Z_cccc", "bbbb1111cccc2222");
    let owned_b = sb.extras("2026-01-03T00-00-00-000Z_cccc");
    let other = sb.session("2026-01-02T00-00-00-000Z_bbbb", "cccc3333dddd4444");
    sb.extras("2026-01-02T00-00-00-000Z_bbbb");
    let before = sb.listing();

    // `a` is listed twice: one result.
    let results = batch(&sb.omp, &[&a, &b, &a]);
    assert_eq!(results.len(), 2, "{results:?}");
    for p in [&a, &b] {
        assert!(result_of(&results, p).is_ok(), "{}", p.display());
        assert!(!p.exists());
    }
    for p in owned_a.iter().chain(&owned_b) {
        assert!(!p.exists(), "{} survived", p.display());
    }
    let expected: Vec<String> = before
        .into_iter()
        .filter(|p| !p.contains("_aaaa") && !p.contains("_cccc"))
        .collect();
    assert_eq!(sb.listing(), expected);
    assert!(other.exists());
    // The file is gone, so deleting it again is an I/O error, not a success.
    let again = batch(&sb.omp, &[&a]);
    assert!(matches!(result_of(&again, &a), Err(DeleteError::Io(_))));
    // An empty batch is a no-op.
    assert!(batch(&sb.omp, &[]).is_empty());
}

#[test]
fn rejects_files_that_are_not_sessions_and_removes_nothing() {
    let Some(sb) = sandbox() else { return };
    let garbage = sb.dir.join("notes.jsonl");
    fs::write(&garbage, "{\"type\":\"message\"}\nnot json\n").unwrap();
    let empty = sb.dir.join("empty.jsonl");
    fs::write(&empty, "").unwrap();
    let text = sb.dir.join("readme.txt");
    fs::write(&text, "hi").unwrap();
    let a_dir = sb.dir.join("dir.jsonl");
    fs::create_dir_all(&a_dir).unwrap();
    let real = sb.session("2026-01-01T00-00-00-000Z_aaaa", "aaaa1111bbbb2222");
    let before = sb.listing();

    let results = batch(&sb.omp, &[&garbage, &empty, &text, &a_dir, &real]);
    for p in [&garbage, &empty] {
        assert!(
            matches!(result_of(&results, p), Err(DeleteError::Changed)),
            "{}",
            p.display()
        );
    }
    for p in [&text, &a_dir] {
        assert!(
            matches!(result_of(&results, p), Err(DeleteError::NotASession)),
            "{}",
            p.display()
        );
    }
    // A real session in the same batch is still deleted.
    assert!(result_of(&results, &real).is_ok());
    assert!(!real.exists());
    let expected: Vec<String> = before
        .into_iter()
        .filter(|p| !p.contains("_aaaa"))
        .collect();
    assert_eq!(sb.listing(), expected);
}

#[test]
fn refuses_a_file_replaced_since_selection_and_keeps_it() {
    let Some(sb) = sandbox() else { return };
    let stem = "2026-01-01T00-00-00-000Z_aaaa";
    let path = sb.session(stem, "aaaa1111bbbb2222");
    let selected = target_of(&path);
    // Another session lands at the same path between selection and deletion.
    sb.session(stem, "ffff9999eeee8888");
    let owned = sb.extras(stem);
    let before = sb.listing();

    let results = run(&sb.omp, std::slice::from_ref(&selected));
    assert!(
        matches!(result_of(&results, &path), Err(DeleteError::Changed)),
        "{results:?}"
    );
    assert!(path.exists());
    assert!(id_of(&path) == "ffff9999eeee8888");
    assert!(owned.iter().all(|p| p.exists()));
    assert_eq!(sb.listing(), before);

    // The replacement, selected afresh, is deletable.
    let results = batch(&sb.omp, &[&path]);
    assert!(result_of(&results, &path).is_ok(), "{results:?}");
    assert!(!path.exists());
}

struct TmuxServer(String);

impl TmuxServer {
    fn run(&self, args: &[&str]) -> bool {
        Command::new("tmux")
            .args(["-L", &self.0, "-f", "/dev/null"])
            .args(args)
            .status()
            .is_ok_and(|s| s.success())
    }
}

impl TmuxServer {
    fn pane(&self) -> String {
        Command::new("tmux")
            .args(["-L", &self.0, "capture-pane", "-p"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default()
    }
}

impl Drop for TmuxServer {
    fn drop(&mut self) {
        let _ = Command::new("tmux")
            .args(["-L", &self.0, "kill-server"])
            .stderr(std::process::Stdio::null())
            .status();
        // tmux leaves the socket file behind after kill-server.
        let uid = Command::new("id").arg("-u").output().ok();
        let uid = uid.map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
        if let Some(uid) = uid {
            let base = std::env::var_os("TMUX_TMPDIR")
                .map_or_else(|| PathBuf::from("/tmp"), PathBuf::from);
            let _ = fs::remove_file(base.join(format!("tmux-{uid}")).join(&self.0));
        }
    }
}

fn lines(p: &Path) -> usize {
    fs::read_to_string(p).map_or(0, |t| t.lines().count())
}

#[test]
fn refuses_a_session_held_by_a_live_omp_then_deletes_after_it_exits() {
    let Some(sb) = sandbox() else { return };
    if Command::new("tmux").arg("-V").output().is_err() {
        eprintln!("tmux not found; skipping live-lease delete_session test");
        return;
    }
    let target = sb.session("2026-01-01T00-00-00-000Z_live", "eeee5555ffff6666");
    // No assistant turn: the live omp then resumes with the sandbox's `fake` model.
    let text = fs::read_to_string(&target).unwrap();
    let without_assistant: String = text
        .lines()
        .filter(|l| !l.contains("\"assistant\""))
        .map(|l| format!("{l}\n"))
        .collect();
    fs::write(&target, without_assistant).unwrap();
    let owned = sb.extras("2026-01-01T00-00-00-000Z_live");
    let sibling = sb.session("2026-01-02T00-00-00-000Z_bbbb", "cccc3333dddd4444");
    let before = sb.listing();

    // omp claims a session's lease on its first write, so the live tab sends one message. The
    // sandbox's only provider is `fake` at 127.0.0.1:1 (connection refused): the request fails
    // locally and fast, after the user message is persisted; no real provider is contacted.
    fs::write(
        sb.agent.join("models.yml"),
        "providers:\n  fake:\n    baseUrl: http://127.0.0.1:1/v1\n    apiKey: sk-fake\n    api: openai-completions\n    models:\n      - id: fake-1\n        name: Fake One\n        contextWindow: 100000\n        maxTokens: 4096\n",
    )
    .unwrap();
    fs::write(
        sb.agent.join("config.yml"),
        "startup:\n  setupWizard: false\nmodelRoles:\n  default: fake/fake-1\n",
    )
    .unwrap();
    let server = TmuxServer(format!("omp-deck-test-del{}", std::process::id()));
    let cmd = format!(
        "exec env PI_CODING_AGENT_DIR={} PI_CONFIG_DIR={} {} \
         --model fake/fake-1 --no-extensions --resume {}",
        sb.agent.display(),
        sb.config.display(),
        sb.omp.program().display(),
        target.display()
    );
    assert!(server.run(&["new-session", "-d", "-x", "120", "-y", "40", &cmd]));
    let start_lines = lines(&target);
    let deadline = Instant::now() + Duration::from_secs(30);
    // Wait for the TUI (the footer names the model), type one message, wait for the write.
    while !server.pane().contains("Fake One") {
        assert!(
            Instant::now() < deadline,
            "omp TUI never came up; pane:\n{}",
            server.pane()
        );
        std::thread::sleep(Duration::from_millis(200));
    }
    assert!(server.run(&["send-keys", "-l", "yo"]));
    assert!(server.run(&["send-keys", "Enter"]));
    while lines(&target) <= start_lines {
        assert!(
            Instant::now() < deadline,
            "live omp never wrote the session; pane:\n{}",
            server.pane()
        );
        std::thread::sleep(Duration::from_millis(200));
    }

    // omp claims the lease before it writes, so the write we waited for implies it is held.
    // One batch: the live session is reported Live (after the retry rounds), while the others in
    // the same batch were deleted in the first round and reported before it.
    let victim = sb.session("2026-01-04T00-00-00-000Z_vvvv", "vvvv1111wwww2222");
    let victim_owned = sb.extras("2026-01-04T00-00-00-000Z_vvvv");
    let notes = sb.dir.join("notes.jsonl");
    fs::write(&notes, "garbage\n").unwrap();
    let started = Instant::now();
    let mut seen: Vec<(PathBuf, Result<(), DeleteError>, Instant)> = Vec::new();
    sb.omp.delete_sessions(
        &[target_of(&target), target_of(&victim), target_of(&notes)],
        |p, r| seen.push((p.to_path_buf(), r, Instant::now())),
    );
    assert_eq!(seen.len(), 3, "{seen:?}");
    let get = |p: &Path| seen.iter().find(|(q, ..)| q == p).unwrap();
    assert!(
        matches!(get(&target).1, Err(DeleteError::Live)),
        "{:?}",
        get(&target).1
    );
    assert!(get(&victim).1.is_ok());
    assert!(matches!(get(&notes).1, Err(DeleteError::Changed)));
    assert!(!victim.exists());
    for p in &victim_owned {
        assert!(!p.exists());
    }
    assert!(
        get(&victim).2 < get(&target).2,
        "the live target is only reported after its retry rounds"
    );
    assert!(started.elapsed() < Duration::from_secs(60));
    assert!(target.exists());
    for p in &owned {
        assert!(p.exists(), "{} removed despite Live", p.display());
    }
    assert!(sibling.exists());
    let during: Vec<String> = sb.listing();
    assert!(before.iter().all(|p| during.contains(p)));

    // Lease is released when omp exits; deletion then succeeds (poll like the app does).
    assert!(server.run(&["kill-server"]));
    let end = Instant::now() + Duration::from_secs(10);
    loop {
        let r = batch(&sb.omp, &[&target]);
        match result_of(&r, &target) {
            Ok(()) => break,
            Err(DeleteError::Live) if Instant::now() < end => {}
            Err(e) => panic!("delete after exit failed: {e:?}"),
        }
    }
    assert!(!target.exists());
    assert!(sibling.exists());
}
