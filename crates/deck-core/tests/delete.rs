//! `Omp::delete_session` against the real `omp` in a throwaway agent dir: removes exactly the
//! session's own files, refuses a session held by a live omp, rejects non-session files.
//! Never contacts a real provider. Skips (with a message) when `omp` or `tmux` is not installed.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use deck_core::omp::{DeleteError, Omp};

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
        eprintln!("omp not found on PATH; skipping delete_session integration test");
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

#[test]
fn deletes_exactly_the_session_and_its_own_files() {
    let Some(sb) = sandbox() else { return };
    let target = sb.session("2026-01-01T00-00-00-000Z_aaaa", "aaaa1111bbbb2222");
    let owned = sb.extras("2026-01-01T00-00-00-000Z_aaaa");
    let other = sb.session("2026-01-02T00-00-00-000Z_bbbb", "cccc3333dddd4444");
    sb.extras("2026-01-02T00-00-00-000Z_bbbb");
    let before = sb.listing();

    sb.omp.delete_session(&target).unwrap();

    assert!(!target.exists());
    for p in &owned {
        assert!(!p.exists(), "{} survived", p.display());
    }
    let expected: Vec<String> = before
        .into_iter()
        .filter(|p| !p.contains("_aaaa"))
        .collect();
    assert_eq!(sb.listing(), expected);
    assert!(other.exists());
    // Idempotence: the file is gone, so a second delete is an I/O error, not a success.
    assert!(matches!(
        sb.omp.delete_session(&target),
        Err(DeleteError::Io(_))
    ));
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
    let before = sb.listing();

    for p in [&garbage, &empty, &text, &a_dir] {
        assert!(
            matches!(sb.omp.delete_session(p), Err(DeleteError::NotASession)),
            "{}",
            p.display()
        );
    }
    assert_eq!(sb.listing(), before);
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
    match sb.omp.delete_session(&target) {
        Err(DeleteError::Live) => {}
        other => panic!("expected Live while omp holds the session, got {other:?}"),
    }
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
        match sb.omp.delete_session(&target) {
            Ok(()) => break,
            Err(DeleteError::Live) if Instant::now() < end => {
                std::thread::sleep(Duration::from_millis(250));
            }
            Err(e) => panic!("delete after exit failed: {e:?}"),
        }
    }
    assert!(!target.exists());
    assert!(sibling.exists());
}
