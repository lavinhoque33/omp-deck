use std::fs::{self, File, FileTimes};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use deck_core::sessions::{
    ProjectGroup, Scanner, SessionWatcher, SessionsRoot, abbreviate_home, find_fork, scan,
};

fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for e in fs::read_dir(from).unwrap() {
        let e = e.unwrap();
        let dest = to.join(e.file_name());
        if e.file_type().unwrap().is_dir() {
            copy_dir(&e.path(), &dest);
        } else {
            fs::copy(e.path(), dest).unwrap();
        }
    }
}

fn set_mtime(path: &Path, t: SystemTime) {
    let f = File::options().write(true).open(path).unwrap();
    f.set_times(FileTimes::new().set_modified(t)).unwrap();
}

/// Day-of-store timestamp derived from the `YYYY-MM-DD` in the fixture file name, so mtime
/// order equals name order across projects (git does not preserve mtimes).
fn mtime_for_day(name: &str) -> Option<SystemTime> {
    let date = name.get(..10)?;
    let mut it = date.split('-');
    let _y: u64 = it.next()?.parse().ok()?;
    let m: u64 = it.next()?.parse().ok()?;
    let d: u64 = it.next()?.parse().ok()?;
    Some(UNIX_EPOCH + Duration::from_secs(1_700_000_000 + (m * 31 + d) * 86_400))
}

fn fixture_store() -> (tempfile::TempDir, SessionsRoot) {
    let dir = tempfile::tempdir().unwrap();
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/store");
    copy_dir(&src, dir.path());
    for p in fs::read_dir(dir.path()).unwrap().flatten() {
        for f in fs::read_dir(p.path()).unwrap().flatten() {
            let name = f.file_name().to_string_lossy().into_owned();
            if let Some(t) = mtime_for_day(&name).filter(|_| name.ends_with(".jsonl")) {
                set_mtime(&f.path(), t);
            }
        }
    }
    let root = SessionsRoot::at(dir.path());
    (dir, root)
}

fn labels(g: &ProjectGroup) -> Vec<&str> {
    g.sessions.iter().map(|s| s.label()).collect()
}

#[test]
fn fixture_store_groups_by_header_cwd_and_orders_by_mtime() {
    let (_d, root) = fixture_store();
    let groups = scan(&root);

    let cwds: Vec<_> = groups.iter().map(|g| g.cwd.to_str().unwrap()).collect();
    // Newest session decides group order: /srv/two (02-03), /srv/one (02-02), alpha (01-06).
    assert_eq!(cwds, ["/srv/two", "/srv/one", "/home/u/alpha"]);

    assert_eq!(
        labels(&groups[2]),
        [
            "cut off prompt works",      // truncated last line tolerated
            "Header only titled",        // title line then header, nothing else
            "plain string prompt",       // string content, assistant line skipped
            "Add retry logic to client", // image-only message skipped, parts joined + collapsed
            "Fix the parser",            // title trimmed, preferred over prompt
        ]
    );
    let titled = &groups[2].sessions[4];
    assert_eq!(titled.title.as_deref(), Some("Fix the parser"));
    assert_eq!(
        titled.first_prompt, None,
        "titled sessions are not searched for a prompt"
    );
    assert_eq!(titled.id, "01a00001-0000-0000-0000-000000000001");

    // Two differently named project dirs with the same header cwd merge into one group.
    assert_eq!(labels(&groups[1]), ["second dir same cwd", "Weird dir one"]);
    assert_eq!(labels(&groups[0]), ["Other project"]);
}

#[test]
fn stubs_garbage_and_subagent_artifacts_are_hidden() {
    let (_d, root) = fixture_store();
    let groups = scan(&root);
    let names: Vec<String> = groups
        .iter()
        .flat_map(|g| &g.sessions)
        .map(|s| s.path.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names.len(), 8);
    for hidden in [
        "01a00004", "01a00007", "garbage", "noheader", "AgentOne", "notes",
    ] {
        assert!(
            !names.iter().any(|n| n.contains(hidden)),
            "{hidden} must not be listed"
        );
    }
}

#[test]
fn display_is_home_abbreviated() {
    let (_d, root) = fixture_store();
    let groups = scan(&root);
    assert_eq!(
        groups[2].display,
        abbreviate_home(Path::new("/home/u/alpha"))
    );
    assert_eq!(groups[0].display, "/srv/two");
}

#[test]
fn abbreviate_home_cases() {
    let Some(home) = dirs::home_dir() else { return };
    assert_eq!(abbreviate_home(&home), "~");
    let sep = std::path::MAIN_SEPARATOR;
    assert_eq!(abbreviate_home(&home.join("x/y")), format!("~{sep}x{sep}y"));
    assert_eq!(
        abbreviate_home(&home.join("x").join("y")),
        format!("~{sep}x{sep}y")
    );
    assert_eq!(abbreviate_home(Path::new("/srv/one")), "/srv/one");
    // A sibling sharing the textual prefix is not inside HOME.
    let mut lookalike = home.clone().into_os_string();
    lookalike.push("2/x");
    assert_eq!(
        abbreviate_home(Path::new(&lookalike)),
        Path::new(&lookalike).display().to_string()
    );
}

#[test]
fn missing_root_is_empty() {
    let d = tempfile::tempdir().unwrap();
    let root = SessionsRoot::at(d.path().join("nope"));
    assert!(!root.exists());
    assert!(scan(&root).is_empty());
}

fn write_session(root: &Path, project: &str, name: &str, body: &[u8]) -> PathBuf {
    let dir = root.join(project);
    fs::create_dir_all(&dir).unwrap();
    let p = dir.join(name);
    fs::write(&p, body).unwrap();
    p
}

fn header(cwd: &str) -> String {
    format!(r#"{{"type":"session","version":3,"id":"id-{cwd}","timestamp":"t","cwd":"{cwd}"}}"#)
        + "\n"
}

fn user_msg(text: &str) -> String {
    format!(r#"{{"type":"message","message":{{"role":"user","content":"{text}"}}}}"#) + "\n"
}

fn junk_line(bytes: usize) -> String {
    format!(
        r#"{{"type":"message","message":{{"role":"toolResult","content":[{{"type":"text","text":"{}"}}]}}}}"#,
        "x".repeat(bytes)
    ) + "\n"
}

#[test]
fn prompt_found_after_long_lines_but_not_beyond_read_budget() {
    let d = tempfile::tempdir().unwrap();
    // 100 KiB and 400 KiB lines (the latter exceeds the per-line cap) before the prompt.
    let body = header("/p/long")
        + &junk_line(100 * 1024)
        + &junk_line(400 * 1024)
        + &user_msg("after the big lines");
    write_session(d.path(), "a", "s1.jsonl", body.as_bytes());

    // Prompt starts past the 2 MiB budget: never read.
    let mut far = header("/p/far");
    for _ in 0..22 {
        far += &junk_line(100 * 1024);
    }
    far += &user_msg("too far away");
    write_session(d.path(), "b", "s2.jsonl", far.as_bytes());

    let groups = scan(&SessionsRoot::at(d.path()));
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].cwd, Path::new("/p/long"));
    assert_eq!(
        groups[0].sessions[0].first_prompt.as_deref(),
        Some("after the big lines")
    );
}

#[test]
fn prompt_truncates_to_200_chars_on_char_boundary() {
    let d = tempfile::tempdir().unwrap();
    let text = "é€ ".repeat(150); // multibyte, whitespace gets collapsed/trimmed
    let body = header("/p/u") + &user_msg(&text);
    write_session(d.path(), "a", "s.jsonl", body.as_bytes());
    let groups = scan(&SessionsRoot::at(d.path()));
    let prompt = groups[0].sessions[0].first_prompt.as_deref().unwrap();
    assert_eq!(prompt.chars().count(), 200);
    assert!(prompt.starts_with("é€ é€ "));
}

#[test]
fn cached_rescan_reads_no_content_and_picks_up_changes() {
    let (d, root) = fixture_store();
    let mut sc = Scanner::new(root);
    let first = sc.scan();
    // 9 files in the alpha dir + 3 in the others; the subagent artifact is never opened.
    assert_eq!(sc.files_parsed(), 12);

    let second = sc.scan();
    assert_eq!(sc.files_parsed(), 12, "unchanged store must not be re-read");
    assert_eq!(first, second);

    // Modification: a stub gains a prompt (len + mtime change).
    let stub = d
        .path()
        .join("-home-u-alpha/2026-01-04T10-00-00-000Z_01a00004.jsonl");
    let mut f = fs::OpenOptions::new().append(true).open(&stub).unwrap();
    f.write_all(user_msg("now it has a prompt").as_bytes())
        .unwrap();
    drop(f);
    set_mtime(&stub, SystemTime::now());
    let third = sc.scan();
    assert_eq!(sc.files_parsed(), 13, "only the changed file is re-read");
    assert_eq!(
        third[0].cwd,
        Path::new("/home/u/alpha"),
        "newest session moves its group first"
    );
    assert_eq!(third[0].sessions[0].label(), "now it has a prompt");

    // Vanished file: dropped from results and evicted, so recreating it re-parses.
    fs::remove_file(&stub).unwrap();
    let fourth = sc.scan();
    assert!(
        fourth
            .iter()
            .flat_map(|g| &g.sessions)
            .all(|s| s.path != stub)
    );
    fs::write(&stub, header("/home/u/alpha") + &user_msg("reborn")).unwrap();
    sc.scan();
    assert_eq!(sc.files_parsed(), 14);
}

fn wait_for(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn watcher_publishes_only_changes_wakes_on_refresh_and_stops_on_drop() {
    let (d, root) = fixture_store();
    let calls = Arc::new(AtomicUsize::new(0));
    let c = Arc::clone(&calls);
    // One-hour interval: any later update can only come from refresh_now().
    let w = SessionWatcher::spawn(root, Duration::from_secs(3600), move || {
        c.fetch_add(1, Ordering::SeqCst);
    });
    wait_for("first snapshot", || w.latest().is_some());
    assert_eq!(w.generation(), 1);
    assert_eq!(w.latest().unwrap().len(), 3);

    // Unchanged store: refresh must not bump the generation or call on_update again.
    w.refresh_now();
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(w.generation(), 1);
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    write_session(
        d.path(),
        "new-project",
        "2026-03-01T00-00-00-000Z_new.jsonl",
        (header("/p/new") + &user_msg("hello")).as_bytes(),
    );
    w.refresh_now();
    wait_for("changed snapshot", || w.generation() == 2);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(w.latest().unwrap().len(), 4);

    let t = Instant::now();
    drop(w);
    assert!(
        t.elapsed() < Duration::from_secs(5),
        "drop must wake the sleeping thread"
    );
}

#[test]
fn relative_root_yields_absolute_entry_paths() {
    // Test cwd is the crate dir; a temp dir inside it is reachable by a relative path.
    let d = tempfile::tempdir_in(".").unwrap();
    write_session(
        d.path(),
        "a",
        "s.jsonl",
        (header("/p/rel") + &user_msg("hi")).as_bytes(),
    );
    let rel = Path::new(".").join(d.path().file_name().unwrap());
    let groups = scan(&SessionsRoot::at(rel));
    let p = &groups[0].sessions[0].path;
    assert!(p.is_absolute(), "{p:?}");
    assert!(!p.components().any(|c| c.as_os_str() == "."), "{p:?}");
    assert!(p.is_file());
}

#[test]
fn watcher_reports_root_existence() {
    let d = tempfile::tempdir().unwrap();
    let w = SessionWatcher::spawn(
        SessionsRoot::at(d.path().join("missing")),
        Duration::from_secs(3600),
        || {},
    );
    wait_for("first snapshot", || w.latest().is_some());
    assert!(!w.root_exists());
    fs::create_dir_all(d.path().join("missing")).unwrap();
    w.refresh_now();
    wait_for("root appears", || w.root_exists());
}

fn fork_file(
    root: &Path,
    project: &str,
    name: &str,
    parent: &str,
    title_first: bool,
    age: u64,
) -> PathBuf {
    let hdr = format!(
        r#"{{"type":"session","version":3,"id":"{name}","cwd":"/p","parentSession":"{parent}"}}"#
    ) + "\n";
    let title = r#"{"type":"title","title":"T"}"#.to_string() + "\n";
    let body = if title_first { title + &hdr } else { hdr };
    let p = write_session(root, project, &format!("{name}.jsonl"), body.as_bytes());
    let t = SystemTime::now() - Duration::from_secs(age);
    File::options()
        .write(true)
        .open(&p)
        .unwrap()
        .set_times(FileTimes::new().set_modified(t))
        .unwrap();
    p
}

#[test]
fn find_fork_picks_newest_match_among_decoys() {
    let d = tempfile::tempdir().unwrap();
    let since = SystemTime::now() - Duration::from_secs(100);
    fork_file(d.path(), "-a", "wrong-parent", "other", false, 5);
    fork_file(d.path(), "-a", "too-old", "par", false, 500);
    fork_file(d.path(), "-a/artifacts", "sub", "par", false, 1);
    fork_file(d.path(), "-b", "older-match", "par", false, 50);
    let newest = fork_file(d.path(), "-b", "newest-match", "par", true, 10);
    let root = SessionsRoot::at(d.path().to_path_buf());
    assert_eq!(find_fork(&root, "par", since), Some(newest));
}

#[test]
fn find_fork_none_when_absent() {
    let d = tempfile::tempdir().unwrap();
    let since = SystemTime::now() - Duration::from_secs(100);
    fork_file(d.path(), "-a", "x", "other", false, 5);
    write_session(d.path(), "-a", "plain.jsonl", header("/p").as_bytes());
    let root = SessionsRoot::at(d.path().to_path_buf());
    assert_eq!(find_fork(&root, "par", since), None);
    assert_eq!(
        find_fork(&SessionsRoot::at(d.path().join("nope")), "par", since),
        None
    );
}
