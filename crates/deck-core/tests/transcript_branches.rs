//! Branch-aware transcript replay and live leaf anchors.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use deck_core::agents::{
    LeafAnchor, TranscriptItem, TranscriptReader, TranscriptUpdate, TranscriptWatcher,
};
use deck_core::tree::SessionTree;
use serde_json::json;

const BRANCH: &[u8] = include_bytes!("fixtures/transcript/branch.jsonl");

fn file(bytes: &[u8]) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    fs::write(&path, bytes).unwrap();
    (dir, path)
}

fn append(path: &Path, text: &str) {
    writeln!(
        OpenOptions::new().append(true).open(path).unwrap(),
        "{text}"
    )
    .unwrap();
}

fn prompt(id: &str, parent: Option<&str>) -> String {
    format!(
        r#"{{"type":"message","id":{},"parentId":{},"message":{{"role":"user","content":{}}}}}"#,
        json!(id),
        json!(parent),
        json!(id)
    )
}

fn anchor(leaf: Option<&str>, tail: Option<&str>) -> LeafAnchor {
    LeafAnchor {
        leaf: leaf.map(str::to_owned),
        tail: tail.map(str::to_owned),
    }
}

fn texts(items: &[TranscriptItem]) -> Vec<&str> {
    items
        .iter()
        .map(|item| match item {
            TranscriptItem::User { text, .. } | TranscriptItem::Assistant { text, .. } => {
                text.as_str()
            }
            _ => panic!("unexpected item: {item:?}"),
        })
        .collect()
}

fn check(reader: &mut TranscriptReader, reset: bool, expected: &[&str]) {
    let update = reader.refresh().unwrap();
    assert_eq!(update.reset, reset, "{update:?}");
    assert_eq!(texts(&update.items), expected);
}

#[test]
fn linear_file_is_unchanged() {
    let (_dir, path) = file(b"");
    append(&path, &prompt("one", None));
    append(&path, &prompt("two", Some("one")));
    append(&path, &prompt("three", Some("two")));
    let mut reader = TranscriptReader::new(path);
    check(&mut reader, false, &["one", "two", "three"]);
}

#[test]
fn branched_file_agrees_with_tree_and_updates_by_path() {
    let (_dir, path) = file(BRANCH);
    let tree = SessionTree::load(&path).unwrap();
    let leaf = tree.file_leaf.unwrap();
    assert_eq!(tree.nodes[leaf].id, "active");
    let mut path_ids = vec![tree.nodes[leaf].id.as_str()];
    let mut parent = tree.parent_index(leaf);
    while let Some(i) = parent {
        path_ids.push(&tree.nodes[i].id);
        parent = tree.parent_index(i);
    }
    path_ids.reverse();

    let mut reader = TranscriptReader::new(path.clone());
    check(&mut reader, false, &path_ids);
    append(&path, &prompt("next", Some("active")));
    check(&mut reader, false, &["next"]);
    append(
        &path,
        r#"{"type":"model_usage","id":"background","parentId":"old","activeLeafId":"next"}"#,
    );
    assert_eq!(reader.refresh().unwrap(), TranscriptUpdate::default());
    append(&path, &prompt("other", Some("root")));
    check(&mut reader, true, &["root", "other"]);
}

#[test]
fn missing_leaf_falls_back_on_load_and_is_ignored_in_appends() {
    let (_dir, path) = file(BRANCH);
    append(
        &path,
        r#"{"type":"model_usage","id":"invalid","parentId":"child","activeLeafId":"not-in-file"}"#,
    );
    let tree = SessionTree::load(&path).unwrap();
    assert_eq!(tree.nodes[tree.file_leaf.unwrap()].id, "active");
    let mut reader = TranscriptReader::new(path.clone());
    check(&mut reader, false, &["root", "active"]);
    append(
        &path,
        r#"{"type":"model_usage","id":"invalid-again","parentId":"child","activeLeafId":"not-in-file"}"#,
    );
    check(&mut reader, false, &[]);
    append(
        &path,
        r#"{"type":"model_usage","id":"clear","parentId":"active","activeLeafId":null}"#,
    );
    assert!(SessionTree::load(&path).unwrap().file_leaf.is_none());
    check(&mut reader, true, &[]);
    check(&mut TranscriptReader::new(path), false, &[]);
}

#[test]
fn unpinned_anchor_reads_to_eof_and_applies_again_after_the_leaf_moves() {
    let (_dir, path) = file(BRANCH);
    let mut reader = TranscriptReader::new(path.clone());
    check(&mut reader, false, &["root", "active"]);
    append(&path, &prompt("not-yet-read", Some("active")));
    let jump = anchor(Some("root"), None);
    reader.anchor(jump.clone());
    check(&mut reader, true, &["root"]);
    append(&path, &prompt("continue", Some("root")));
    check(&mut reader, false, &["continue"]);
    append(&path, &prompt("other-branch", Some("old")));
    check(&mut reader, true, &["root", "abandoned", "other-branch"]);
    reader.anchor(jump.clone());
    check(&mut reader, true, &["root"]);
    reader.anchor(jump);
    check(&mut reader, false, &[]);
    reader.anchor(anchor(None, None));
    check(&mut reader, true, &[]);
}

#[test]
fn pinned_anchor_folds_newer_lines_and_waits_for_a_future_tail() {
    let (_dir, path) = file(BRANCH);
    let mut reader = TranscriptReader::new(path.clone());
    check(&mut reader, false, &["root", "active"]);
    reader.anchor(anchor(Some("root"), Some("child")));
    check(&mut reader, false, &[]);

    reader.anchor(anchor(Some("root"), Some("future")));
    check(&mut reader, false, &[]);
    append(&path, &prompt("between", Some("active")));
    check(&mut reader, false, &["between"]);
    append(&path, &prompt("future", Some("between")));
    check(&mut reader, true, &["root"]);
    append(&path, &prompt("continue", Some("root")));
    check(&mut reader, false, &["continue"]);
}

#[test]
fn old_tail_anchor_folds_past_an_unresolved_leaf_effect() {
    let (_dir, path) = file(BRANCH);
    let mut reader = TranscriptReader::new(path.clone());
    check(&mut reader, false, &["root", "active"]);
    append(&path, &prompt("next", Some("active")));
    append(
        &path,
        r#"{"type":"model_usage","id":"background","parentId":"old","activeLeafId":"not-in-file"}"#,
    );
    check(&mut reader, false, &["next"]);

    reader.anchor(anchor(Some("root"), None));
    check(&mut reader, true, &["root"]);
    reader.anchor(anchor(Some("old"), Some("usage")));
    check(&mut reader, false, &["active", "next"]);
}

#[test]
fn invalid_and_old_file_anchors_are_dropped() {
    let (dir, path) = file(BRANCH);
    let mut reader = TranscriptReader::new(path.clone());
    check(&mut reader, false, &["root", "active"]);
    reader.anchor(anchor(Some("missing"), None));
    check(&mut reader, false, &[]);

    reader.anchor(anchor(Some("replacement"), Some("tail")));
    check(&mut reader, false, &[]);
    let new = dir.path().join("new.jsonl");
    fs::write(
        &new,
        format!(
            "{}\n{}\n",
            prompt("replacement", None),
            prompt("tail", Some("replacement"))
        ),
    )
    .unwrap();
    fs::rename(new, &path).unwrap();
    check(&mut reader, true, &["replacement", "tail"]);

    reader.anchor(anchor(Some("replacement"), None));
    fs::write(&path, format!("{}\n", prompt("shrunk", None))).unwrap();
    check(&mut reader, true, &["shrunk"]);
}

#[test]
fn oversized_ancestor_keeps_its_parent_link_and_late_metadata() {
    let (_dir, path) = file(b"");
    append(&path, &prompt("root", None));
    let mut out = OpenOptions::new().append(true).open(&path).unwrap();
    out.write_all(br#"{"type":"message","message":{"role":"assistant","content":""#)
        .unwrap();
    let mb = vec![b'x'; 1 << 20];
    for _ in 0..57 {
        out.write_all(&mb).unwrap();
    }
    writeln!(out, r#""}},"id":"oversized","parentId":"root"}}"#).unwrap();
    drop(out);
    append(&path, &prompt("child", Some("oversized")));
    check(&mut TranscriptReader::new(path), false, &["root", "child"]);
}

#[test]
fn watcher_publishes_the_reset_epoch_with_the_new_snapshot() {
    let (_dir, path) = file(BRANCH);
    let (tx, rx) = mpsc::channel();
    let watcher = TranscriptWatcher::spawn(path, move || {
        let _ = tx.send(());
    });
    rx.recv_timeout(Duration::from_secs(3)).unwrap();
    let (epoch, old) = watcher.latest_with_epoch();
    assert_eq!(epoch, 0);
    assert_eq!(texts(&old), ["root", "active"]);
    let revision = watcher.revision();

    watcher.anchor(anchor(Some("root"), None));
    rx.recv_timeout(Duration::from_secs(3)).unwrap();
    let (epoch, new) = watcher.latest_with_epoch();
    assert_eq!(watcher.epoch(), 1);
    assert_eq!(epoch, 1);
    assert_eq!(watcher.revision(), revision + 1);
    assert_eq!(texts(&new), ["root"]);
    assert_eq!(
        texts(&old),
        ["root", "active"],
        "older snapshots stay immutable"
    );
}

#[test]
fn watcher_applies_its_initial_anchor_before_publishing() {
    let (_dir, path) = file(BRANCH);
    let (tx, rx) = mpsc::channel();
    let watcher = TranscriptWatcher::spawn_anchored(
        path,
        Some(anchor(Some("old"), Some("usage"))),
        move || {
            let _ = tx.send(());
        },
    );
    rx.recv_timeout(Duration::from_secs(3)).unwrap();
    let (epoch, items) = watcher.latest_with_epoch();
    assert_eq!(epoch, 0);
    assert_eq!(watcher.revision(), 1);
    assert_eq!(texts(&items), ["root", "abandoned"]);
}

#[test]
fn watcher_publishes_an_initial_empty_leaf_as_loaded() {
    let (_dir, path) = file(BRANCH);
    let (tx, rx) = mpsc::channel();
    let watcher =
        TranscriptWatcher::spawn_anchored(path, Some(anchor(None, Some("usage"))), move || {
            let _ = tx.send(());
        });
    rx.recv_timeout(Duration::from_secs(3)).unwrap();
    let (epoch, items) = watcher.latest_with_epoch();
    assert_eq!(epoch, 0);
    assert_eq!(watcher.revision(), 1);
    assert!(items.is_empty());
}
