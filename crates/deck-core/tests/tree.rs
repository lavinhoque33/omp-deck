//! Session tree: parsing rules against synthetic fixtures, leaf computation, and the watcher.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use deck_core::tree::{NodeKind, SessionTree, TreeNode, TreeWatcher};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/tree")
        .join(name)
}

fn load(name: &str) -> SessionTree {
    SessionTree::load(&fixture(name)).unwrap()
}

fn node<'a>(t: &'a SessionTree, id: &str) -> &'a TreeNode {
    &t.nodes[t.index[id]]
}

fn text<'a>(t: &'a SessionTree, id: &str) -> &'a str {
    &node(t, id).text
}

/// A session file with the given entry lines (header first).
fn session(dir: &Path, name: &str, lines: &[&str]) -> PathBuf {
    let path = dir.join(name);
    let mut body = String::from(r#"{"type":"session","version":3,"id":"s","cwd":"/w"}"#);
    for l in lines {
        body.push('\n');
        body.push_str(l);
    }
    body.push('\n');
    fs::write(&path, body).unwrap();
    path
}

fn msg(id: &str, parent: Option<&str>, role: &str, extra: &str) -> String {
    let parent = parent.map_or("null".to_owned(), |p| format!("\"{p}\""));
    format!(
        r#"{{"type":"message","id":"{id}","parentId":{parent},"timestamp":"2026-03-03T10:00:00.000Z"{extra},"message":{{"role":"{role}","content":"x"}}}}"#
    )
}

fn leaf_id(lines: &[String]) -> Option<String> {
    let tmp = tempfile::tempdir().unwrap();
    let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
    let t = SessionTree::load(&session(tmp.path(), "s.jsonl", &refs)).unwrap();
    t.file_leaf.map(|i| t.nodes[i].id.clone())
}

#[test]
fn leaf_is_last_entry_unless_active_leaf_says_otherwise() {
    let (a, b, c) = (
        msg("a", None, "user", ""),
        msg("b", Some("a"), "assistant", ""),
        msg("c", Some("b"), "user", ""),
    );
    // plain: the last physical entry, even a label or branch_summary
    assert_eq!(
        leaf_id(&[a.clone(), b.clone(), c.clone()]).as_deref(),
        Some("c")
    );
    let label =
        r#"{"type":"label","id":"l","parentId":"c","timestamp":"t","targetId":"a","label":"x"}"#;
    assert_eq!(
        leaf_id(&[a.clone(), b.clone(), c.clone(), label.into()]).as_deref(),
        Some("l")
    );
    // activeLeafId on the last entry pointing at an existing entry wins
    let off = msg("d", Some("a"), "assistant", r#","activeLeafId":"b""#);
    assert_eq!(
        leaf_id(&[a.clone(), b.clone(), c.clone(), off]).as_deref(),
        Some("b")
    );
    // equal to the entry's own id
    let own = msg("d", Some("c"), "assistant", r#","activeLeafId":"d""#);
    assert_eq!(
        leaf_id(&[a.clone(), b.clone(), c.clone(), own]).as_deref(),
        Some("d")
    );
    // explicit null: no leaf
    let null = msg("d", Some("c"), "assistant", r#","activeLeafId":null"#);
    assert_eq!(leaf_id(&[a.clone(), b.clone(), c.clone(), null]), None);
    // dangling id: the last entry that is its own leaf (here `c`; `d` points elsewhere)
    let dangling = msg("d", Some("a"), "assistant", r#","activeLeafId":"nope""#);
    assert_eq!(leaf_id(&[a, b, c, dangling]).as_deref(), Some("c"));
    // dangling id with no self-leaf entry at all
    let only = msg("d", None, "user", r#","activeLeafId":"nope""#);
    assert_eq!(leaf_id(&[only]), None);
    assert_eq!(leaf_id(&[]), None);
}

#[test]
fn children_sort_by_timestamp_and_roots_cover_missing_and_self_parents() {
    let t = load("rich.jsonl");
    let ids = |v: &[usize]| {
        v.iter()
            .map(|&i| t.nodes[i].id.as_str())
            .collect::<Vec<_>>()
    };
    // `u3` precedes `u2` in the file but is newer
    assert_eq!(ids(&t.children[t.index["a2"]]), ["u2", "u3"]);
    assert_eq!(ids(&t.roots), ["m1"]);

    let r = load("roots.jsonl");
    let ids = |v: &[usize]| {
        v.iter()
            .map(|&i| r.nodes[i].id.as_str())
            .collect::<Vec<_>>()
    };
    // null parent, dangling parent, self parent: all roots, in file order; duplicate id and
    // the malformed line are skipped
    assert_eq!(ids(&r.roots), ["ra", "rb", "rc"]);
    assert_eq!(r.nodes.len(), 5);
    assert_eq!(node(&r, "ra").text, "root a");
    assert_eq!(ids(&r.children[r.index["rb"]]), ["ub"]);
    assert!(
        r.children[r.index["rc"]].is_empty(),
        "self parent is not its own child"
    );
    assert_eq!(r.parent_index(r.index["rc"]), None);
}

#[test]
fn file_leaf_of_the_rich_fixture_follows_active_leaf_and_ignores_the_truncated_line() {
    let t = load("rich.jsonl");
    // last complete entry is a model_usage whose activeLeafId is `a5`; the truncated `zz` is not a node
    assert!(!t.index.contains_key("zz"));
    assert_eq!(t.nodes[t.file_leaf.unwrap()].id, "a5");
    assert_eq!(node(&t, "mu1").active_leaf, Some(Some("a5".into())));
    assert_eq!(node(&t, "u1").active_leaf, None);
}

#[test]
fn active_path_marks_root_to_leaf() {
    let t = load("rich.jsonl");
    let on = t.active_path(t.file_leaf);
    let marked: Vec<&str> = t
        .nodes
        .iter()
        .zip(&on)
        .filter(|&(_, &b)| b)
        .map(|(n, _)| n.id.as_str())
        .collect();
    assert_eq!(
        marked,
        [
            "m1", "t1", "u1", "a1", "r1", "r2", "a2", "u3", "a4", "c1", "bs1", "l1", "l2", "l3",
            "l4", "cm1", "cm2", "ct1", "a5"
        ]
    );
    assert!(t.active_path(None).iter().all(|&b| !b));
    assert!(
        !on[t.index["u2"]] && !on[t.index["a3"]],
        "other branch is off the path"
    );
}

#[test]
fn labels_latest_wins_and_cleared_are_removed() {
    let t = load("rich.jsonl");
    assert_eq!(t.labels.get("a2").map(String::as_str), Some("final"));
    assert!(!t.labels.contains_key("u1"), "set then cleared");
    assert_eq!(t.labels.len(), 1);
    assert_eq!(text(&t, "l3"), "[label: first]");
    assert_eq!(text(&t, "l4"), "[label: (cleared)]");
}

#[test]
fn row_text_for_each_kind() {
    let t = load("rich.jsonl");
    let k = |id: &str| node(&t, id).kind.clone();
    // messages: role prefix is not part of the text; \n and \t fold to spaces
    assert_eq!(
        (k("u1"), text(&t, "u1")),
        (NodeKind::User, "Hello world foo")
    );
    assert_eq!(text(&t, "u2"), "second question as string");
    assert_eq!(
        (k("a1"), text(&t, "a1")),
        (NodeKind::Assistant, "Sure, reading")
    );
    assert_eq!(text(&t, "a2"), "Done.");
    // toolResult rows come from the matching toolCall's arguments
    assert_eq!(
        (k("r1"), text(&t, "r1")),
        (NodeKind::ToolResult, "[read: /tmp/x.rs:10-14]")
    );
    assert_eq!(text(&t, "r2"), "[bash: ls -la]");
    // assistant without text
    assert!(!node(&t, "a3").has_text);
    assert_eq!(text(&t, "a3"), "(no content)");
    assert_eq!(text(&t, "a4"), "rate limited");
    assert_eq!(node(&t, "a4").stop_reason.as_deref(), Some("error"));
    assert_eq!(text(&t, "a5"), "(aborted)");
    assert!(node(&t, "a1").has_text && node(&t, "a2").has_text);
    assert_eq!(k("bx"), NodeKind::BashExecution);
    assert_eq!(text(&t, "bx"), "[bash]: git status --short");
    assert_eq!(
        (k("dv"), text(&t, "dv")),
        (NodeKind::Developer, "be careful")
    );
    assert_eq!(
        (k("ot"), text(&t, "ot")),
        (NodeKind::OtherRole("custom".into()), "[custom]")
    );
    // other entry types
    assert_eq!(
        (k("c1"), text(&t, "c1")),
        (NodeKind::Compaction, "[compaction: 12k tokens]")
    );
    assert_eq!(
        (k("bs1"), text(&t, "bs1")),
        (NodeKind::BranchSummary, "[branch summary]: Explored X")
    );
    assert_eq!(
        (k("cm1"), text(&t, "cm1")),
        (
            NodeKind::CustomMessage("async-result".into()),
            "[async-result]: Job done"
        )
    );
    assert_eq!(text(&t, "cm2"), "advisor (sec, blocker): check bounds");
    assert_eq!(
        (k("m1"), text(&t, "m1")),
        (NodeKind::ModelChange, "[model: anthropic/claude-x]")
    );
    assert_eq!(
        (k("t1"), text(&t, "t1")),
        (NodeKind::ThinkingChange, "[thinking: high]")
    );
    assert_eq!(
        (k("mu1"), text(&t, "mu1")),
        (NodeKind::ModelUsage, "[model usage: title smol p/m]")
    );
    assert_eq!(
        (k("ct1"), text(&t, "ct1")),
        (
            NodeKind::Custom("tool_execution_start".into()),
            "[custom: tool_execution_start]"
        )
    );
    assert_eq!(k("l1"), NodeKind::Label);
    let setting = |id: &str, ty: &str, txt: &str| {
        assert_eq!(k(id), NodeKind::Setting(ty.into()), "{id}");
        assert_eq!(text(&t, id), txt, "{id}");
    };
    setting(
        "st1",
        "service_tier_change",
        "[service tier: openai:priority]",
    );
    setting("tc", "title_change", "[title: New title]");
    setting("mc", "mode_change", "[mode: plan]");
    setting("cp", "credential_pin", "[credential pin: anthropic]");
    setting("si", "session_init", "[session init]");
    setting("ti", "ttsr_injection", "[ttsr injection]");
    setting("rb", "reset_boundary", "[reset boundary]");
}

#[test]
fn row_text_is_capped_and_unmatched_tool_results_fall_back() {
    let tmp = tempfile::tempdir().unwrap();
    let long = "é".repeat(500);
    let p = session(
        tmp.path(),
        "s.jsonl",
        &[
            &format!(
                r#"{{"type":"message","id":"u","parentId":null,"timestamp":"t","message":{{"role":"user","content":[{{"type":"text","text":"{long}"}}]}}}}"#
            ),
            r#"{"type":"message","id":"r","parentId":"u","timestamp":"t","message":{"role":"toolResult","toolCallId":"gone","toolName":"grep","content":"x"}}"#,
            r#"{"type":"message","id":"r2","parentId":"r","timestamp":"t","message":{"role":"toolResult","content":"x"}}"#,
        ],
    );
    let t = SessionTree::load(&p).unwrap();
    assert_eq!(text(&t, "u").chars().count(), 200);
    assert_eq!(text(&t, "r"), "[grep]");
    assert_eq!(text(&t, "r2"), "[tool]");
}

#[test]
fn fork_target_and_forkability() {
    let t = load("rich.jsonl");
    let target = |id: &str| t.fork_target(t.index[id]);
    // user → nearest message ancestor ("fork before this prompt"); the first prompt only has
    // settings entries (thinking level, model) above it, so there is nothing to fork before
    assert_eq!(target("u2"), Some("a2"));
    assert_eq!(target("u1"), None);
    assert!(!t.is_forkable(t.index["u1"]));
    // assistant / toolResult → itself ("fork after this reply")
    assert_eq!(target("a2"), Some("a2"));
    assert_eq!(target("r1"), Some("r1"));
    assert_eq!(target("bx"), Some("bx"));
    // non-message entries are never forkable
    for id in ["c1", "bs1", "l1", "cm1", "ct1", "m1", "mu1"] {
        assert!(!t.is_forkable(t.index[id]), "{id}");
        assert_eq!(target(id), None, "{id}");
    }
    // a user message without a resolvable parent cannot be forked before
    let r = load("roots.jsonl");
    assert!(!r.is_forkable(r.index["ra"]));
    assert!(!r.is_forkable(r.index["rb"]), "dangling parent");
    assert_eq!(r.fork_target(r.index["rb"]), None);
    assert!(r.is_forkable(r.index["ub"]));
    assert_eq!(r.fork_target(r.index["ub"]), Some("rb"));
    assert!(r.is_forkable(r.index["aa"]));
    assert!(!r.is_forkable(99));
}

fn entry(ty: &str, id: &str, parent: Option<&str>, extra: &str) -> String {
    let parent = parent.map_or("null".to_owned(), |p| format!("\"{p}\""));
    format!(
        r#"{{"type":"{ty}","id":"{id}","parentId":{parent},"timestamp":"2026-03-03T10:00:00.000Z"{extra}}}"#
    )
}

#[test]
fn user_fork_target_skips_non_message_ancestors() {
    let tmp = tempfile::tempdir().unwrap();
    let lines = [
        entry("model_change", "m", None, ""),
        msg("u0", Some("m"), "user", ""),
        msg("a0", Some("u0"), "assistant", ""),
        entry("title_change", "tt", Some("a0"), r#","title":"x""#),
        entry("thinking_level_change", "th", Some("tt"), ""),
        entry("compaction", "cp", Some("th"), ""),
        msg("u1", Some("cp"), "user", ""),
        // a parent cycle of settings entries with a prompt hanging off it
        entry("model_change", "x", Some("y"), ""),
        entry("model_change", "y", Some("x"), ""),
        msg("u2", Some("x"), "user", ""),
    ];
    let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
    let t = SessionTree::load(&session(tmp.path(), "s.jsonl", &refs)).unwrap();
    assert_eq!(t.fork_target(t.index["u1"]), Some("a0"));
    assert!(t.is_forkable(t.index["u1"]));
    // only settings above: not forkable
    assert_eq!(t.fork_target(t.index["u0"]), None);
    assert!(!t.is_forkable(t.index["u0"]));
    assert!(!t.is_forkable(t.index["u2"]), "cycle without a message");
    // other message kinds fork at themselves
    assert_eq!(t.fork_target(t.index["a0"]), Some("a0"));
}

#[test]
fn oversized_entry_keeps_its_top_level_active_leaf() {
    let tmp = tempfile::tempdir().unwrap();
    let a = msg("a", None, "user", "");
    let b = msg("b", Some("a"), "assistant", "");
    // > MAX_LINE (4 MiB); nested decoy `activeLeafId` inside the body must not count
    let pad = "z".repeat(5 * 1024 * 1024);
    let big = |id: &str, tail: &str| {
        format!(
            r#"{{"type":"model_usage","id":"{id}","parentId":"b","timestamp":"2026-03-03T10:00:01.000Z","usage":{{"activeLeafId":"b","note":"{pad}"}}{tail}}}"#
        )
    };
    let at_end = session(
        tmp.path(),
        "end.jsonl",
        &[&a, &b, &big("big", r#","activeLeafId":"a""#)],
    );
    let t = SessionTree::load(&at_end).unwrap();
    assert!(node(&t, "big").text.starts_with("[large entry"));
    assert_eq!(
        node(&t, "big").active_leaf,
        Some(Some("a".to_owned())),
        "scanned past the buffered prefix"
    );
    assert_eq!(t.file_leaf.map(|i| t.nodes[i].id.as_str()), Some("a"));

    let null = session(
        tmp.path(),
        "null.jsonl",
        &[&a, &b, &big("big", r#","activeLeafId":null"#)],
    );
    let t = SessionTree::load(&null).unwrap();
    assert_eq!(node(&t, "big").active_leaf, Some(None));
    assert_eq!(t.file_leaf, None);

    let absent = session(tmp.path(), "none.jsonl", &[&a, &b, &big("big", "")]);
    let t = SessionTree::load(&absent).unwrap();
    assert_eq!(node(&t, "big").active_leaf, None);
    assert_eq!(t.file_leaf.map(|i| t.nodes[i].id.as_str()), Some("big"));
}

// ---- watcher -----------------------------------------------------------------------------------

fn wait_for(what: &str, mut f: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(10);
    while Instant::now() < end {
        if f() {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for {what}");
}

fn append(path: &Path, s: &str) {
    OpenOptions::new()
        .append(true)
        .open(path)
        .unwrap()
        .write_all(s.as_bytes())
        .unwrap();
}

fn ids(t: &SessionTree) -> Vec<&str> {
    t.nodes.iter().map(|n| n.id.as_str()).collect()
}

#[test]
fn watcher_tails_appends_waits_for_truncated_lines_and_reloads_replacements() {
    let tmp = tempfile::tempdir().unwrap();
    let a = msg("a", None, "user", "");
    let path = session(tmp.path(), "s.jsonl", &[&a]);
    let updates = Arc::new(AtomicUsize::new(0));
    let u = Arc::clone(&updates);
    let w = TreeWatcher::spawn(path.clone(), move || {
        u.fetch_add(1, Ordering::SeqCst);
    });
    wait_for("first tree", || w.latest().is_some());
    assert_eq!(ids(&w.latest().unwrap()), ["a"]);
    assert_eq!(w.error(), None);
    let rev = w.revision();
    assert!(rev > 0);
    assert!(updates.load(Ordering::SeqCst) >= 1);

    // append: complete line → picked up; the old snapshot stays valid
    let first = w.latest().unwrap();
    let b = msg("b", Some("a"), "assistant", "");
    append(&path, &format!("{b}\n"));
    wait_for("append", || w.latest().is_some_and(|t| t.nodes.len() == 2));
    assert_eq!(ids(&first), ["a"]);
    assert!(w.revision() > rev);

    // sharing: the new snapshot reuses the old nodes instead of copying them
    let second = w.latest().unwrap();
    assert!(Arc::ptr_eq(&first.nodes[0], &second.nodes[0]));
    assert_eq!(*second.children[second.index["a"]], [second.index["b"]]);
    assert_eq!(second.roots, [0]);

    // a half-written line is not a node (and not an error) until its newline arrives
    let c = msg("c", Some("b"), "user", "");
    let (head, tail) = c.split_at(c.len() / 2);
    append(&path, head);
    std::thread::sleep(Duration::from_millis(700));
    assert_eq!(w.latest().unwrap().nodes.len(), 2);
    assert_eq!(w.error(), None);
    append(&path, &format!("{tail}\n"));
    wait_for("completed line", || {
        w.latest().is_some_and(|t| t.nodes.len() == 3)
    });
    let t = w.latest().unwrap();
    assert_eq!(ids(&t), ["a", "b", "c"]);
    assert_eq!(t.file_leaf.map(|i| t.nodes[i].id.as_str()), Some("c"));
    assert_eq!(*t.children[t.index["a"]], [t.index["b"]]);

    // an unchanged file does not notify
    let rev = w.revision();
    std::thread::sleep(Duration::from_millis(600));
    assert_eq!(w.revision(), rev);

    // replaced by a different file (rename over it, as /move does): full reload
    let other = session(
        tmp.path(),
        "other.jsonl",
        &[
            &msg("x", None, "user", ""),
            &msg("y", Some("x"), "user", ""),
        ],
    );
    fs::rename(&other, &path).unwrap();
    wait_for("replace", || {
        w.latest().is_some_and(|t| ids(&t) == ["x", "y"])
    });

    // shrunk in place: full reload
    fs::write(&path, format!("{}\n", msg("z", None, "user", ""))).unwrap();
    wait_for("shrink", || w.latest().is_some_and(|t| ids(&t) == ["z"]));
}

#[test]
fn watcher_reports_a_missing_file_then_recovers_and_drops_promptly() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("later.jsonl");
    let w = TreeWatcher::spawn(path.clone(), || {});
    wait_for("error", || w.error().is_some());
    assert!(w.latest().is_none());
    session(tmp.path(), "later.jsonl", &[&msg("a", None, "user", "")]);
    wait_for("recovery", || w.latest().is_some() && w.error().is_none());

    let t0 = Instant::now();
    drop(w);
    assert!(
        t0.elapsed() < Duration::from_millis(200),
        "drop joins promptly"
    );
}

#[test]
fn oversized_lines_become_prefix_only_nodes_without_hiding_their_neighbours() {
    let tmp = tempfile::tempdir().unwrap();
    let big = "x".repeat(5 * 1024 * 1024);
    let huge = format!(
        r#"{{"type":"message","id":"big","parentId":"a","timestamp":"2026-03-03T10:00:01.000Z","message":{{"role":"assistant","content":[{{"type":"text","text":"{big}"}}],"stopReason":"stop"}}}}"#
    );
    let p = session(
        tmp.path(),
        "s.jsonl",
        &[
            &msg("a", None, "user", ""),
            &huge,
            &msg("c", Some("big"), "user", ""),
        ],
    );
    let t = SessionTree::load(&p).unwrap();
    assert_eq!(ids(&t), ["a", "big", "c"]);
    let n = node(&t, "big");
    assert_eq!(n.kind, NodeKind::Assistant);
    assert_eq!(n.parent.as_deref(), Some("a"));
    assert_eq!(n.text, "[large entry: 5.0 MB]");
    assert!(n.has_text);
    assert_eq!(t.file_leaf.map(|i| t.nodes[i].id.as_str()), Some("c"));
}

#[test]
fn incremental_appends_equal_a_fresh_load_including_late_parents_and_ties() {
    let tmp = tempfile::tempdir().unwrap();
    let a = msg("a", None, "user", "");
    let path = session(tmp.path(), "s.jsonl", &[&a]);
    let w = TreeWatcher::spawn(path.clone(), || {});
    wait_for("first tree", || w.latest().is_some());
    let before = w.latest().unwrap();

    // children before their parent (`late`), an older sibling arriving after a newer one, equal
    // timestamps, a dangling parent and a self parent
    let older = r#","timestamp":"2026-03-03T09:00:00.000Z""#;
    let lines = [
        msg("c2", Some("late"), "user", ""),
        msg("c1", Some("late"), "user", ""),
        msg("late", Some("a"), "assistant", ""),
        msg("s1", Some("a"), "user", ""),
        msg("s0", Some("a"), "user", older)
            .replace(r#""timestamp":"2026-03-03T10:00:00.000Z","#, ""),
        msg("dang", Some("nowhere"), "user", ""),
        msg("self", Some("self"), "user", ""),
    ];
    append(&path, &(lines.join("\n") + "\n"));
    wait_for("appends", || w.latest().is_some_and(|t| t.nodes.len() == 8));
    let got = w.latest().unwrap();
    let fresh = SessionTree::load(&path).unwrap();
    assert_eq!(*got, fresh);
    assert_eq!(ids(&before), ["a"], "previous snapshot untouched");
    assert_eq!(before.children[0].len(), 0);
    assert_eq!(
        got.roots
            .iter()
            .map(|&i| got.nodes[i].id.as_str())
            .collect::<Vec<_>>(),
        ["a", "dang", "self"]
    );
}
