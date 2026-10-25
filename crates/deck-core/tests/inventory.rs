//! Inventory scan and statistics against a fixture store (`tests/fixtures/inventory`).

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use deck_core::inventory::{InventoryEntry, StatsCache, scan};
use deck_core::sessions::SessionsRoot;
use deck_core::status::TurnOutcome;

struct Store {
    _tmp: tempfile::TempDir,
    root: SessionsRoot,
}

fn copy_dir(from: &Path, to: &Path, cwd: &str) {
    fs::create_dir_all(to).unwrap();
    for e in fs::read_dir(from).unwrap() {
        let e = e.unwrap();
        let dest = to.join(e.file_name());
        if e.file_type().unwrap().is_dir() {
            copy_dir(&e.path(), &dest, cwd);
        } else {
            let text = fs::read(e.path()).unwrap();
            let text = String::from_utf8_lossy(&text).replace("@CWD@", cwd);
            fs::write(dest, text).unwrap();
        }
    }
}

/// The fixture store with every `@CWD@` replaced by an existing directory.
fn store() -> Store {
    let tmp = tempfile::tempdir().unwrap();
    let cwd_dir = tmp.path().join("work");
    fs::create_dir_all(&cwd_dir).unwrap();
    let json = serde_json::to_string(&cwd_dir.to_string_lossy()).unwrap();
    let escaped = &json[1..json.len() - 1];
    let root = tmp.path().join("sessions");
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/inventory");
    copy_dir(&src, &root, escaped);
    Store {
        root: SessionsRoot { path: root },
        _tmp: tmp,
    }
}

fn find<'a>(entries: &'a [InventoryEntry], id: &str) -> &'a InventoryEntry {
    entries
        .iter()
        .find(|e| e.id == id)
        .unwrap_or_else(|| panic!("no entry {id}"))
}

fn len(p: &Path) -> u64 {
    fs::metadata(p).unwrap().len()
}

/// `UNIX_EPOCH + secs`.
fn at(secs: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(secs)
}

#[test]
fn scan_reports_sizes_and_header_facts_for_every_session_with_a_header() {
    let s = store();
    let entries = scan(&s.root);
    let mut ids: Vec<&str> = entries.iter().map(|e| e.id.as_str()).collect();
    ids.sort_unstable();
    // Subagent transcripts, the headerless file and notes.txt are not sessions; the stub is.
    assert_eq!(
        ids,
        ["01c00001", "01c00002", "01c00003", "01c00004", "01c00005"]
    );

    let main = find(&entries, "01c00001");
    let dir = main.artifact_dir();
    let bak = main.path.with_file_name(format!(
        "{}.1.bak",
        main.path.file_name().unwrap().to_string_lossy()
    ));
    let artifacts = len(&dir.join("Agent.jsonl"))
        + len(&dir.join("nested/Deep.jsonl"))
        + len(&dir.join("artifacts/a.txt"))
        + len(&bak);
    assert_eq!(main.jsonl_bytes, len(&main.path));
    assert_eq!(main.artifact_bytes, artifacts);
    assert_eq!(
        main.artifact_files, 4,
        "two transcripts, one artifact, one .bak"
    );
    assert_eq!(
        main.subagents, 2,
        "nested transcripts count, the .bak does not"
    );
    assert_eq!(main.total_bytes(), main.jsonl_bytes + artifacts);
    assert_eq!(main.title.as_deref(), Some("Refactor parser"));
    assert_eq!(
        main.first_prompt, None,
        "a title short-circuits the prompt search"
    );
    assert!(main.cwd_exists && !main.is_stub());
    assert_eq!(main.created, Some(at(1_769_940_000)));

    let stub = find(&entries, "01c00002");
    assert!(stub.is_stub());
    assert_eq!(
        (stub.artifact_bytes, stub.artifact_files, stub.subagents),
        (0, 0, 0)
    );

    // A fork names its parent by id: resolved to the parent's file. A parent given as a
    // path is kept as written even when the file is gone.
    let fork = find(&entries, "01c00003");
    assert_eq!(fork.parent.as_deref(), Some(main.path.as_path()));
    assert_eq!(fork.first_prompt.as_deref(), Some("forked prompt"));
    let lost = find(&entries, "01c00005");
    // "/gone/parent.jsonl" is only an absolute path on Unix.
    #[cfg(unix)]
    assert_eq!(
        lost.parent.as_deref(),
        Some(Path::new("/gone/parent.jsonl"))
    );
    assert!(!lost.cwd_exists);
    assert!(!lost.is_stub());

    // Newest modification first.
    assert!(entries.windows(2).all(|w| w[0].modified >= w[1].modified));
}

#[cfg(unix)]
#[test]
fn scan_never_follows_symlinks_in_the_sibling_directory() {
    let s = store();
    let outside = s._tmp.path().join("outside");
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("big.bin"), vec![0u8; 100_000]).unwrap();
    let before = find(&scan(&s.root), "01c00001").clone();
    std::os::unix::fs::symlink(&outside, before.artifact_dir().join("link")).unwrap();
    std::os::unix::fs::symlink(
        outside.join("big.bin"),
        before.artifact_dir().join("l.jsonl"),
    )
    .unwrap();
    let after = find(&scan(&s.root), "01c00001").clone();
    assert!(
        after.artifact_bytes < before.artifact_bytes + 1000,
        "{} -> {}",
        before.artifact_bytes,
        after.artifact_bytes
    );
    assert_eq!(after.artifact_files, before.artifact_files + 2);
}

#[test]
fn scan_of_a_missing_root_is_empty() {
    let root = SessionsRoot {
        path: PathBuf::from("/nonexistent/ompdeck-inventory-root"),
    };
    assert!(scan(&root).is_empty());
}

#[test]
fn stats_sum_main_usage_and_subagent_cost() {
    let s = store();
    let entries = scan(&s.root);
    let main = find(&entries, "01c00001");
    let mut cache = StatsCache::new();
    let st = cache.compute(main).unwrap();
    assert_eq!((st.user_messages, st.assistant_turns), (2, 2));
    assert_eq!((st.input_tokens, st.output_tokens), (307, 53));
    assert_eq!((st.cache_read_tokens, st.cache_write_tokens), (50, 5));
    assert!((st.cost - 0.76).abs() < 1e-9, "{}", st.cost);
    assert!(
        (st.subagent_cost - 0.5).abs() < 1e-9,
        "{}",
        st.subagent_cost
    );
    assert_eq!(st.models, ["m-a", "m-b"]);
    assert_eq!(st.compactions, 1);
    assert_eq!(st.last_outcome, TurnOutcome::Aborted);
    assert_eq!(st.last_activity, Some(at(1_769_940_006)));
    assert_eq!(cache.get(&main.path), Some(&st));

    // A stub has nothing to count.
    let stub = cache.compute(find(&entries, "01c00002")).unwrap();
    assert_eq!((stub.user_messages, stub.assistant_turns), (0, 0));
    assert_eq!(stub.last_outcome, TurnOutcome::Unknown);
}

#[test]
fn unchanged_files_are_not_parsed_again_and_appends_parse_only_the_tail() {
    let s = store();
    let entries = scan(&s.root);
    let e = find(&entries, "01c00001").clone();
    let mut cache = StatsCache::new();
    let first = cache.compute(&e).unwrap();
    let full = cache.parsed_bytes();
    assert_eq!(
        full,
        len(&e.path)
            + len(&e.artifact_dir().join("Agent.jsonl"))
            + len(&e.artifact_dir().join("nested/Deep.jsonl"))
    );
    assert_eq!(cache.compute(&e).unwrap(), first);
    assert_eq!(
        cache.parsed_bytes(),
        full,
        "nothing changed: nothing parsed"
    );

    let line = concat!(
        r#"{"type":"message","id":"a3","parentId":null,"timestamp":"2026-02-01T11:00:00.000Z","message":{"role":"assistant","content":"x","model":"m-c","#,
        r#""usage":{"input":1,"output":2,"cacheRead":0,"cacheWrite":0,"cost":{"total":1.5}},"stopReason":"stop"}}"#,
        "\n"
    );
    OpenOptions::new()
        .append(true)
        .open(&e.path)
        .unwrap()
        .write_all(line.as_bytes())
        .unwrap();
    let second = cache.compute(&e).unwrap();
    assert_eq!(
        cache.parsed_bytes() - full,
        line.len() as u64,
        "only the appended line"
    );
    assert_eq!(second.assistant_turns, first.assistant_turns + 1);
    assert_eq!(second.input_tokens, first.input_tokens + 1);
    assert!((second.cost - first.cost - 1.5).abs() < 1e-9);
    assert_eq!(second.last_outcome, TurnOutcome::Completed);
    assert_eq!(second.models, ["m-a", "m-b", "m-c"]);
    assert!(second.last_activity > first.last_activity);

    // A subagent transcript that grows is picked up too, again by its tail only.
    let sub = e.artifact_dir().join("Agent.jsonl");
    let before = cache.parsed_bytes();
    let sub_line = line.replace("1.5", "0.25");
    OpenOptions::new()
        .append(true)
        .open(&sub)
        .unwrap()
        .write_all(sub_line.as_bytes())
        .unwrap();
    let third = cache.compute(&e).unwrap();
    assert_eq!(cache.parsed_bytes() - before, sub_line.len() as u64);
    assert!((third.subagent_cost - 0.75).abs() < 1e-9);
    assert_eq!(
        third.assistant_turns, second.assistant_turns,
        "main numbers untouched"
    );
}

#[test]
fn a_truncated_last_line_waits_for_its_end_and_is_then_counted_once() {
    let s = store();
    let entries = scan(&s.root);
    let e = find(&entries, "01c00004").clone();
    let mut cache = StatsCache::new();
    let cut = cache.compute(&e).unwrap();
    assert_eq!((cut.user_messages, cut.assistant_turns), (1, 1));
    assert_eq!(cut.input_tokens, 5);
    let complete_lines = len(&e.path)
        - fs::read_to_string(&e.path)
            .unwrap()
            .lines()
            .last()
            .unwrap()
            .len() as u64;
    assert_eq!(
        cache.parsed_bytes(),
        complete_lines,
        "the partial line is not consumed"
    );

    let rest = ",\"output\":1,\"cost\":{\"total\":0.4}},\"stopReason\":\"error\",\"errorMessage\":\"boom\"}}\n";
    OpenOptions::new()
        .append(true)
        .open(&e.path)
        .unwrap()
        .write_all(rest.as_bytes())
        .unwrap();
    let done = cache.compute(&e).unwrap();
    assert_eq!(done.assistant_turns, 2);
    assert_eq!(done.input_tokens, 14);
    assert!((done.cost - 0.5).abs() < 1e-9);
    assert_eq!(done.last_outcome, TurnOutcome::Error(Some("boom".into())));
    assert_eq!(
        cache.parsed_bytes(),
        len(&e.path),
        "every byte parsed exactly once"
    );
}

#[test]
fn a_rewritten_file_is_parsed_from_scratch() {
    let s = store();
    let entries = scan(&s.root);
    let e = find(&entries, "01c00001").clone();
    let mut cache = StatsCache::new();
    cache.compute(&e).unwrap();
    // Same prefix length, different content, longer file: the prefix check must reject the
    // cached offset.
    let text = fs::read_to_string(&e.path).unwrap();
    let rewritten = text.replace("\"input\":100", "\"input\":900") + "\n";
    fs::write(&e.path, rewritten).unwrap();
    let st = cache.compute(&e).unwrap();
    assert_eq!(st.input_tokens, 1107);
    assert_eq!(st.assistant_turns, 2, "no double counting");
}
/// A session of 80 assistant lines (`input` = line number, >8 KiB) behind a padded title line,
/// the way omp writes it.
fn big_session(s: &Store, title: &str) -> (PathBuf, String) {
    let dir = s.root.path.join("proj-big");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("2026-03-01T10-00-00-000Z_01c0ffee.jsonl");
    let cwd = s._tmp.path().join("work");
    let mut text = title_line(title);
    text += &format!(
        "{}\n",
        serde_json::json!({"type":"session","version":3,"id":"01c0ffee","timestamp":"2026-03-01T10:00:00.000Z","cwd":cwd})
    );
    text += "{\"type\":\"message\",\"id\":\"u\",\"parentId\":null,\"timestamp\":\"2026-03-01T10:00:01.000Z\",\"message\":{\"role\":\"user\",\"content\":\"hi\",\"timestamp\":1}}\n";
    for i in 10..90 {
        text += &assistant(i);
    }
    fs::write(&path, &text).unwrap();
    (path, text)
}

/// The title line in its fixed 256-byte slot (newline included).
fn title_line(title: &str) -> String {
    let head = format!(r#"{{"type":"title","title":"{title}","pad":""#);
    let pad = 256 - head.len() - 3;
    format!("{head}{}\"}}\n", " ".repeat(pad))
}

fn assistant(input: u32) -> String {
    format!(
        "{{\"type\":\"message\",\"id\":\"a{input}\",\"parentId\":null,\"timestamp\":\"2026-03-01T10:00:02.000Z\",\"message\":{{\"role\":\"assistant\",\"content\":\"x\",\"model\":\"m\",\"usage\":{{\"input\":{input},\"output\":1,\"cacheRead\":0,\"cacheWrite\":0,\"cost\":{{\"total\":0.5}}}},\"stopReason\":\"stop\"}}}}\n"
    )
}

fn big_entry(s: &Store) -> InventoryEntry {
    find(&scan(&s.root), "01c0ffee").clone()
}

#[test]
fn a_middle_rewrite_of_a_large_file_followed_by_an_append_is_parsed_from_scratch() {
    let s = store();
    let (path, text) = big_session(&s, "t");
    assert!(text.len() > 16 * 1024);
    let e = big_entry(&s);
    let mut cache = StatsCache::new();
    let first = cache.compute(&e).unwrap();
    assert_eq!(first.input_tokens, (10..90).sum::<u32>() as u64);
    let parsed = cache.parsed_bytes();

    // Same length, different bytes, far from both the head and the tail window.
    let mid = text.find(&assistant(30)).unwrap();
    let mut bytes = text.into_bytes();
    bytes[mid..mid + assistant(30).len()].copy_from_slice(assistant(31).as_bytes());
    fs::write(&path, &bytes).unwrap();
    OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(assistant(100).as_bytes())
        .unwrap();
    let st = cache.compute(&e).unwrap();
    assert_eq!(
        st.input_tokens,
        ((10..90).sum::<u32>() - 30 + 31 + 100) as u64,
        "the rewritten line counts as rewritten, the append once"
    );
    assert_eq!(st.assistant_turns, 81);
    assert_eq!(
        cache.parsed_bytes() - parsed,
        len(&path),
        "a full reparse, not a continuation"
    );
}

#[test]
fn a_title_rewrite_in_place_then_an_append_only_parses_the_tail() {
    let s = store();
    let (path, text) = big_session(&s, "old");
    let e = big_entry(&s);
    let mut cache = StatsCache::new();
    let first = cache.compute(&e).unwrap();
    let parsed = cache.parsed_bytes();

    // omp rewrites the title slot in place, then appends.
    let mut bytes = text.into_bytes();
    let title = title_line("new title");
    assert_eq!(title.len(), 256);
    bytes[..256].copy_from_slice(title.as_bytes());
    fs::write(&path, &bytes).unwrap();
    let line = assistant(100);
    OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(line.as_bytes())
        .unwrap();
    let st = cache.compute(&e).unwrap();
    assert_eq!(st.input_tokens, first.input_tokens + 100);
    assert_eq!(st.assistant_turns, first.assistant_turns + 1);
    assert_eq!(cache.parsed_bytes() - parsed, line.len() as u64);
}

#[cfg(unix)]
#[test]
fn scan_ignores_symlinked_project_dirs_and_symlinked_jsonl_files() {
    let s = store();
    let before = scan(&s.root).len();
    let outside = s._tmp.path().join("outside");
    fs::create_dir_all(&outside).unwrap();
    let cwd = s._tmp.path().join("work");
    let session = |id: &str| {
        format!(
            "{}\n",
            serde_json::json!({"type":"session","version":3,"id":id,"timestamp":"2026-03-01T10:00:00.000Z","cwd":cwd})
        )
    };
    fs::write(outside.join("elsewhere.jsonl"), session("01d00001")).unwrap();
    fs::write(outside.join("real.jsonl"), session("01d00002")).unwrap();
    // A symlinked project dir and a symlinked top-level jsonl in a real project dir.
    std::os::unix::fs::symlink(&outside, s.root.path.join("linked-project")).unwrap();
    std::os::unix::fs::symlink(
        outside.join("real.jsonl"),
        s.root.path.join("proj-b/linked.jsonl"),
    )
    .unwrap();
    let entries = scan(&s.root);
    assert_eq!(entries.len(), before, "{entries:?}");
    assert!(entries.iter().all(|e| !e.id.starts_with("01d")));
}

#[test]
fn compute_many_reports_every_entry_and_matches_compute() {
    let s = store();
    let entries = scan(&s.root);
    let mut many = StatsCache::new();
    let mut seen: Vec<PathBuf> = Vec::new();
    many.compute_many(&entries, 3, &AtomicBool::new(false), |e, r| {
        r.unwrap();
        seen.push(e.path.clone());
    });
    seen.sort();
    let mut all: Vec<PathBuf> = entries.iter().map(|e| e.path.clone()).collect();
    all.sort();
    assert_eq!(seen, all);

    let mut one = StatsCache::new();
    for e in &entries {
        assert_eq!(
            many.get(&e.path),
            Some(&one.compute(e).unwrap()),
            "{}",
            e.id
        );
    }

    // A raised cancel flag starts nothing new.
    let mut cancelled = StatsCache::new();
    let mut n = 0;
    cancelled.compute_many(&entries, 2, &AtomicBool::new(true), |_, _| n += 1);
    assert_eq!(n, 0);

    // A vanished file is reported as an error for that entry only.
    fs::remove_file(&entries[0].path).unwrap();
    let mut errs = 0;
    let mut oks = 0;
    StatsCache::new().compute_many(&entries, 2, &AtomicBool::new(false), |_, r| {
        if r.is_ok() { oks += 1 } else { errs += 1 }
    });
    assert_eq!((errs, oks), (1, entries.len() - 1));
}

#[test]
fn prune_forgets_sessions_that_left_the_inventory() {
    let s = store();
    let entries = scan(&s.root);
    let mut cache = StatsCache::new();
    cache.compute_many(&entries, 2, &AtomicBool::new(false), |_, _| {});
    let main = find(&entries, "01c00001").clone();
    let keep: Vec<InventoryEntry> = entries
        .iter()
        .filter(|e| e.id != main.id)
        .cloned()
        .collect();
    cache.prune(&keep);
    assert!(cache.get(&main.path).is_none());
    assert!(cache.get(&keep[0].path).is_some());
    // The pruned session's files are parsed afresh.
    let before = cache.parsed_bytes();
    cache.compute(&main).unwrap();
    assert!(cache.parsed_bytes() > before + len(&main.path));
}
