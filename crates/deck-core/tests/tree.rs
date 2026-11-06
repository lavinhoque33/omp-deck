//! Session tree: parsing rules against synthetic fixtures, leaf computation, and the watcher.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use deck_core::tree::{
    EntryDetail, LineSpan, NodeKind, RecordedCost, SessionTree, TreeNode, TreeWatcher, read_entry,
};

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

/// A message whose `message` object gets `message_extra` (e.g. `,"usage":{…}`) after its content.
fn msg_with(id: &str, parent: Option<&str>, role: &str, message_extra: &str) -> String {
    msg(id, parent, role, "").replace(
        r#""content":"x"}"#,
        &format!(r#""content":"x"{message_extra}}}"#),
    )
}

fn load_lines(lines: &[String]) -> SessionTree {
    let tmp = tempfile::tempdir().unwrap();
    let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
    SessionTree::load(&session(tmp.path(), "s.jsonl", &refs)).unwrap()
}

fn leaf_id(lines: &[String]) -> Option<String> {
    let t = load_lines(lines);
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

// ---- recorded cost -----------------------------------------------------------------------------

/// `,"usage":{…}` with the given `cost.total` JSON (token fields and cost parts are ignored).
fn usage(total: &str) -> String {
    format!(r#","usage":{{"input":3,"output":4,"cost":{{"input":9,"total":{total}}}}}"#)
}

fn bot(id: &str, parent: Option<&str>, message_extra: &str) -> String {
    msg_with(id, parent, "assistant", message_extra)
}

fn before(t: &SessionTree, id: &str) -> RecordedCost {
    node(t, id).cost_before
}

fn rc(total: f64, incomplete: bool) -> RecordedCost {
    RecordedCost { total, incomplete }
}

#[test]
fn cost_before_is_an_exclusive_file_order_prefix_over_all_branches() {
    let t = load_lines(&[
        msg("u", None, "user", ""),
        bot("a1", Some("u"), &usage("1")),
        // hidden usage record on the a1 branch
        entry(
            "model_usage",
            "mu",
            Some("a1"),
            r#","purpose":"title","usage":{"cost":{"total":0.5}}"#,
        ),
        // a sibling branch still counts: physical order, not the active path
        bot("b1", Some("u"), &usage("0.25")),
        entry("compaction", "cp", Some("b1"), r#","tokensBefore":1000"#),
        bot("a3", Some("cp"), &usage("2")),
        msg("end", Some("a3"), "user", ""),
    ]);
    assert_eq!(before(&t, "u"), rc(0.0, false));
    assert_eq!(before(&t, "a1"), rc(0.0, false), "own usage excluded");
    assert_eq!(before(&t, "mu"), rc(1.0, false));
    assert_eq!(before(&t, "b1"), rc(1.5, false));
    assert_eq!(before(&t, "cp"), rc(1.75, false));
    assert_eq!(
        before(&t, "a3"),
        rc(1.75, false),
        "compaction does not reset"
    );
    assert_eq!(before(&t, "end"), rc(3.75, false));
}

#[test]
fn cost_before_ignores_costs_outside_usage_records() {
    let t = load_lines(&[
        msg_with("u", None, "user", r#","usage":{"cost":{"total":100}}"#),
        bot(
            "a1",
            Some("u"),
            r#","usage":{"nested":{"cost":{"total":50}},"cost":{"total":1,"parts":{"total":40}}}"#,
        ),
        // ordinary tool results never bill, whatever their details say
        msg_with(
            "tr",
            Some("a1"),
            "toolResult",
            r#","toolName":"read","details":{"usage":{"cost":{"total":100}}}"#,
        ),
        // a duplicate id is skipped, its usage with it
        bot("a1", Some("u"), &usage("64")),
        entry(
            "custom_message",
            "cm",
            Some("tr"),
            r#","customType":"x","content":"y","details":{"usage":{"cost":{"total":7}}},"usage":{"cost":{"total":7}}"#,
        ),
        entry(
            "model_usage",
            "mu",
            Some("cm"),
            r#","details":{"usage":{"cost":{"total":8}}},"usage":{"cost":{"total":0.5}}"#,
        ),
        msg("end", Some("mu"), "user", ""),
    ]);
    assert_eq!(ids(&t), ["u", "a1", "tr", "cm", "mu", "end"]);
    assert_eq!(before(&t, "a1"), rc(0.0, false), "user usage ignored");
    assert_eq!(before(&t, "tr"), rc(1.0, false), "only usage.cost.total");
    assert_eq!(before(&t, "cm"), rc(1.0, false));
    assert_eq!(before(&t, "mu"), rc(1.0, false));
    assert_eq!(before(&t, "end"), rc(1.5, false));
}

#[test]
fn legacy_task_results_count_their_details_usage_once() {
    let task = |id: &str, details: &str| {
        // `details` before `toolName`: field order must not matter
        msg_with(
            id,
            Some("a"),
            "toolResult",
            &format!(r#"{details},"toolName":"task""#),
        )
    };
    let t = load_lines(&[
        bot("a", None, &usage("1")),
        task(
            "t1",
            r#","details":{"output":"done","usage":{"cost":{"total":0.5}}}"#,
        ),
        task("t2", ""), // async-style result: no details at all
        task("t3", r#","details":{"async":true}"#),
        task("t4", r#","details":{"usage":null}"#),
        msg("end", Some("t4"), "user", ""),
        task("t5", r#","details":{"usage":{"input":3}}"#), // usage without a cost
        msg("end2", Some("t5"), "user", ""),
    ]);
    assert_eq!(before(&t, "t1"), rc(1.0, false));
    assert_eq!(before(&t, "t2"), rc(1.5, false));
    assert_eq!(before(&t, "end"), rc(1.5, false), "no usage is not unknown");
    assert_eq!(before(&t, "end2"), rc(1.5, true));
}

/// `cost_before` of a probe entry after a 1.0 assistant and `record`.
fn cost_after(record: &str) -> RecordedCost {
    let t = load_lines(&[
        bot("a0", None, &usage("1")),
        record.to_owned(),
        msg("end", Some("a0"), "user", ""),
    ]);
    before(&t, "end")
}

#[test]
fn unknown_costs_mark_the_prefix_incomplete_without_losing_the_known_sum() {
    let a = |extra: &str| bot("x", Some("a0"), extra);
    let mu = |extra: &str| entry("model_usage", "x", Some("a0"), extra);
    let unknown = [
        a(""),
        a(r#","usage":null"#),
        a(r#","usage":"lots""#),
        a(r#","usage":{"input":5}"#),
        a(r#","usage":{"cost":null}"#),
        a(r#","usage":{"cost":3}"#),
        a(r#","usage":{"cost":{"input":0.5}}"#),
        a(&usage("null")),
        a(&usage(r#""0.5""#)),
        a(&usage("-1")),
        a(&usage(r#"{"usd":1}"#)),
        mu(""),
        mu(r#","usage":{"cost":{"total":[1]}}"#),
        // malformed complete line: skipped, may have been billed
        r#"{"type":"message","id":"bad","message":"#.to_owned(),
    ];
    for record in &unknown {
        assert_eq!(cost_after(record), rc(1.0, true), "{record}");
    }
    let known = [
        (a(&usage("0")), 1.0),
        (a(&usage("2")), 3.0),
        (mu(&usage("0.5")), 1.5),
        (msg("x", Some("a0"), "user", ""), 1.0),
        (msg("x", Some("a0"), "toolResult", ""), 1.0),
        (msg("x", Some("a0"), "bashExecution", ""), 1.0),
        (entry("title_change", "x", Some("a0"), ""), 1.0),
    ];
    for (record, total) in &known {
        assert_eq!(cost_after(record), rc(*total, false), "{record}");
    }

    // incomplete is sticky while later known costs keep adding
    let t = load_lines(&[
        bot("a0", None, &usage("1")),
        bot("x", Some("a0"), ""),
        bot("y", Some("x"), &usage("0.5")),
        msg("end", Some("y"), "user", ""),
    ]);
    assert_eq!(before(&t, "y"), rc(1.0, true));
    assert_eq!(before(&t, "end"), rc(1.5, true));
}

#[test]
fn cost_overflow_keeps_the_known_sum_and_marks_incomplete() {
    let t = load_lines(&[
        bot("a0", None, &usage("1.5e308")),
        bot("a1", Some("a0"), &usage("1.5e308")),
        bot("a2", Some("a1"), &usage("0.5")),
        msg("end", Some("a2"), "user", ""),
    ]);
    assert_eq!(before(&t, "a1"), rc(1.5e308, false));
    let end = before(&t, "end");
    assert!(end.total.is_finite() && end.total >= 1.5e308, "{end:?}");
    assert!(end.incomplete);
}

#[test]
fn oversized_lines_are_unknown_cost_only_when_they_may_be_usage_records() {
    let tmp = tempfile::tempdir().unwrap();
    let pad = "z".repeat(5 * 1024 * 1024);
    let big = |id: &str, message: &str| {
        format!(
            r#"{{"type":"message","id":"{id}","parentId":"a0","timestamp":"2026-03-03T10:00:01.000Z","message":{{{message},"content":[{{"type":"text","text":"{pad}"}}]}}}}"#
        )
    };
    let p = session(
        tmp.path(),
        "s.jsonl",
        &[
            &bot("a0", None, &usage("1")),
            &big("bu", r#""role":"user""#),
            &big(
                "bt",
                r#""role":"toolResult","toolCallId":"c","toolName":"read""#,
            ),
            &msg("p1", Some("a0"), "user", ""),
            &big("ba", r#""role":"assistant""#),
            &msg("p2", Some("a0"), "user", ""),
        ],
    );
    let t = SessionTree::load(&p).unwrap();
    assert_eq!(ids(&t), ["a0", "bu", "bt", "p1", "ba", "p2"]);
    assert_eq!(before(&t, "p1"), rc(1.0, false));
    assert_eq!(before(&t, "ba"), rc(1.0, false));
    assert_eq!(before(&t, "p2"), rc(1.0, true));
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

#[test]
fn watcher_cost_prefix_matches_a_fresh_load_and_restarts_on_replacement() {
    let tmp = tempfile::tempdir().unwrap();
    let path = session(
        tmp.path(),
        "s.jsonl",
        &[
            &msg("u", None, "user", ""),
            &bot("a", Some("u"), &usage("1")),
        ],
    );
    let w = TreeWatcher::spawn(path.clone(), || {});
    wait_for("first tree", || w.latest().is_some());
    let first = w.latest().unwrap();

    // hidden usage on another branch, then a billable line written in two halves
    let mu = entry("model_usage", "mu", Some("u"), &usage("0.5"));
    let b = bot("b", Some("a"), &usage("0.25"));
    let (head, tail) = b.split_at(b.len() / 2);
    append(&path, &format!("{mu}\n{head}"));
    wait_for("model usage", || {
        w.latest().is_some_and(|t| t.nodes.len() == 3)
    });
    std::thread::sleep(Duration::from_millis(600));
    append(
        &path,
        &format!("{tail}\n{}\n", msg("end", Some("b"), "user", "")),
    );
    wait_for("rest", || w.latest().is_some_and(|t| t.nodes.len() == 5));
    let second = w.latest().unwrap();
    assert_eq!(*second, SessionTree::load(&path).unwrap());
    assert_eq!(before(&second, "b"), rc(1.5, false));
    assert_eq!(
        before(&second, "end"),
        rc(1.75, false),
        "a half-written line is not a skipped one"
    );
    assert!(Arc::ptr_eq(&first.nodes[1], &second.nodes[1]));
    assert_eq!(ids(&first), ["u", "a"]);

    // a malformed complete line: later entries become incomplete, as in a fresh load
    append(
        &path,
        &format!(
            "{{\"type\":\"message\",\n{}\n",
            msg("late", Some("end"), "user", "")
        ),
    );
    wait_for("late", || w.latest().is_some_and(|t| t.nodes.len() == 6));
    let third = w.latest().unwrap();
    assert_eq!(*third, SessionTree::load(&path).unwrap());
    assert_eq!(before(&third, "late"), rc(1.75, true));

    // replaced: the prefix restarts from the new file, old snapshots keep theirs
    let other = session(
        tmp.path(),
        "other.jsonl",
        &[
            &bot("x", None, &usage("2")),
            &msg("y", Some("x"), "user", ""),
        ],
    );
    fs::rename(&other, &path).unwrap();
    wait_for("replace", || {
        w.latest().is_some_and(|t| ids(&t) == ["x", "y"])
    });
    let t = w.latest().unwrap();
    assert_eq!(*t, SessionTree::load(&path).unwrap());
    assert_eq!(before(&t, "y"), rc(2.0, false));
    assert_eq!(before(&second, "end"), rc(1.75, false));
    assert_eq!(before(&third, "late"), rc(1.75, true));
}

// ---- line spans + read_entry -------------------------------------------------------------------

/// Byte offsets of every line (without newlines) in `body`.
fn spans_of(body: &str) -> Vec<LineSpan> {
    let mut out = Vec::new();
    let mut off = 0u64;
    for line in body.split_inclusive('\n') {
        let content = line.strip_suffix('\n').unwrap_or(line);
        out.push(LineSpan {
            offset: off,
            len: content.len() as u64,
        });
        off += line.len() as u64;
    }
    out
}

#[test]
fn spans_address_exactly_each_line() {
    let tmp = tempfile::tempdir().unwrap();
    let a = msg("a", None, "user", "");
    let b = msg("b", Some("a"), "assistant", "");
    let path = session(tmp.path(), "s.jsonl", &[&a, &b]);
    let body = fs::read_to_string(&path).unwrap();
    let spans = spans_of(&body);
    // Line 0 is the session header (no node); lines 1.. are a, b.
    let t = SessionTree::load(&path).unwrap();
    assert_eq!(node(&t, "a").line, Some(spans[1]));
    assert_eq!(node(&t, "b").line, Some(spans[2]));
    for id in ["a", "b"] {
        let span = node(&t, id).line.unwrap();
        let got = &body.as_bytes()[span.offset as usize..(span.offset + span.len) as usize];
        assert!(got.starts_with(format!(r#"{{"type":"message","id":"{id}""#).as_bytes()));
    }
}

#[test]
fn spans_stay_correct_across_incremental_appends_and_truncated_tails() {
    let tmp = tempfile::tempdir().unwrap();
    let a = msg("a", None, "user", "");
    let path = session(tmp.path(), "s.jsonl", &[&a]);
    let w = TreeWatcher::spawn(path.clone(), || {});
    wait_for("first tree", || w.latest().is_some());
    let b = msg("b", Some("a"), "assistant", "");
    let (head, tail) = b.split_at(b.len() / 2);
    append(&path, head);
    std::thread::sleep(Duration::from_millis(600));
    assert_eq!(w.latest().unwrap().nodes.len(), 1);
    append(&path, &format!("{tail}\n"));
    let c = msg("c", Some("b"), "user", "");
    append(&path, &format!("{c}\n"));
    wait_for("appends", || w.latest().is_some_and(|t| t.nodes.len() == 3));
    let got = w.latest().unwrap();
    let fresh = SessionTree::load(&path).unwrap();
    assert_eq!(*got, fresh);
    let body = fs::read_to_string(&path).unwrap();
    for id in ["a", "b", "c"] {
        let span = node(&got, id).line.expect("a span");
        let got_line = &body.as_bytes()[span.offset as usize..(span.offset + span.len) as usize];
        assert!(got_line.starts_with(format!(r#"{{"type":"message","id":"{id}""#).as_bytes()));
    }
}

#[test]
fn read_entry_returns_prompts_assistant_calls_and_notices() {
    use deck_core::agents::TranscriptItem;
    let tmp = tempfile::tempdir().unwrap();
    let user = r#"{"type":"message","id":"u","parentId":null,"timestamp":"t","message":{"role":"user","content":[{"type":"text","text":"fix login"}]}}"#.to_owned();
    let assistant = r#"{"type":"message","id":"a","parentId":"u","timestamp":"t","message":{"role":"assistant","content":[{"type":"text","text":"reading"},{"type":"thinking","thinking":"hmm"},{"type":"toolCall","id":"c1","name":"read","arguments":{"path":"/x"}}],"stopReason":"toolUse"}}"#.to_owned();
    let r1 = r#"{"type":"message","id":"r1","parentId":"a","timestamp":"t","message":{"role":"toolResult","toolCallId":"c1","toolName":"read","content":[{"type":"text","text":"ok"}]}}"#.to_owned();
    let r2 = r#"{"type":"message","id":"r2","parentId":"r1","timestamp":"t","message":{"role":"toolResult","toolCallId":"c1","toolName":"read","content":[{"type":"text","text":"ok again"}]}}"#.to_owned();
    let hidden = r#"{"type":"custom_message","id":"h","parentId":"r2","timestamp":"t","customType":"m","content":"secret","display":false}"#.to_owned();
    let summary = r#"{"type":"branch_summary","id":"bs","parentId":"h","timestamp":"t","summary":"did things"}"#.to_owned();
    let refs: Vec<&str> = [&user, &assistant, &r1, &r2, &hidden, &summary]
        .iter()
        .map(|s| s.as_str())
        .collect();
    let path = session(tmp.path(), "s.jsonl", &refs);
    let t = SessionTree::load(&path).unwrap();
    let detail = |id: &str| read_entry(&path, &t, t.index[id]).unwrap();
    assert!(!detail("u").truncated);
    assert!(matches!(
        &detail("u").items[..],
        [TranscriptItem::User { text, .. }] if text == "fix login"
    ));
    let a = detail("a");
    assert!(
        a.items
            .iter()
            .any(|i| matches!(i, TranscriptItem::Assistant { text, .. } if text == "reading"))
    );
    assert!(
        a.items
            .iter()
            .any(|i| matches!(i, TranscriptItem::Thinking { text } if text == "hmm"))
    );
    assert!(
        a.items
            .iter()
            .any(|i| matches!(i, TranscriptItem::ToolCall { call_id, .. } if call_id == "c1"))
    );
    // Parallel results: each result is preceded by its call from the assistant ancestor.
    for id in ["r1", "r2"] {
        let d = detail(id);
        assert_eq!(d.items.len(), 2, "{id}: {d:?}");
        assert!(matches!(
            &d.items[0],
            TranscriptItem::ToolCall { call_id, .. } if call_id == "c1"
        ));
        assert!(matches!(&d.items[1], TranscriptItem::ToolResult { .. }));
    }
    assert!(matches!(
        &detail("h").items[..],
        [TranscriptItem::Notice { kind, text }] if kind == "m" && text == "secret"
    ));
    assert!(matches!(
        &detail("bs").items[..],
        [TranscriptItem::Notice { kind, text }] if kind == "branch_summary" && text == "did things"
    ));
    // Model/thinking/label rows carry no content: empty items.
    let model = r#"{"type":"model_change","id":"m","parentId":"bs","timestamp":"t","model":"a/b"}"#;
    let path2 = session(tmp.path(), "s2.jsonl", &[&user, model]);
    let t2 = SessionTree::load(&path2).unwrap();
    assert_eq!(
        read_entry(&path2, &t2, t2.index["m"]).unwrap().items.len(),
        0
    );
}

#[test]
fn read_entry_rejects_replaced_files_and_caps_huge_lines() {
    let tmp = tempfile::tempdir().unwrap();
    let a = msg("a", None, "user", "");
    let path = session(tmp.path(), "s.jsonl", &[&a]);
    let t = SessionTree::load(&path).unwrap();
    // Replaced file: the span now addresses a different id.
    fs::write(
        &path,
        format!(
            "{}\n{}\n",
            r#"{"type":"session","version":3,"id":"s","cwd":"/w"}"#,
            msg("b", None, "user", "")
        ),
    )
    .unwrap();
    let err = read_entry(&path, &t, t.index["a"]).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    assert_eq!(err.to_string(), "session file changed");
    // Over the 8 MiB cap: empty + truncated (span kept, from a synthetic tree).
    let mut huge = t.clone();
    let i = huge.index["a"];
    let mut replaced = (*huge.nodes[i]).clone();
    replaced.line = Some(LineSpan {
        offset: 0,
        len: 9 * 1024 * 1024,
    });
    huge.nodes[i] = Arc::new(replaced);
    let d: EntryDetail = read_entry(&path, &huge, i).unwrap();
    assert!(d.truncated && d.items.is_empty());
}

#[test]
fn row_text_folds_carriage_returns_and_unicode_separators() {
    let line = |text: &str| {
        let v = serde_json::json!({
            "type": "message", "id": "u", "parentId": null,
            "message": {"role": "user", "content": [{"type": "text", "text": text}]},
        });
        load_lines(&[v.to_string()])
    };
    assert_eq!(line("a\rb\nc\td e f").nodes[0].text, "a b c d e f");
    let t = load("rich.jsonl");
    assert_eq!(t.labels.get("a2").map(String::as_str), Some("final"));
}
