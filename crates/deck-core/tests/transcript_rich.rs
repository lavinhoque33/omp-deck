//! Rich transcript items: structured args, details, images and `read_blob`.

use std::fs;
use std::path::{Path, PathBuf};

use deck_core::agents::{ImageRef, TranscriptItem, TranscriptReader, read_blob};
use serde_json::json;

const PNG_ID: &str = "929e08d597feae56";
const JPG_ID: &str = "131afd4cbe2f7ea4";
const PNG_BYTES: [u8; 8] = [0x89, b'P', b'N', b'G', 13, 10, 26, 10];

fn rich() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/transcript/rich.jsonl")
}

fn read_all(path: &Path) -> Vec<TranscriptItem> {
    TranscriptReader::new(path.to_path_buf())
        .refresh()
        .unwrap()
        .items
}

/// A message line laid out like omp writes it (`type` first, `role` first in the message);
/// serde_json's own key order would put them last.
fn message_line(role: &str, fields: serde_json::Value) -> String {
    let body = fields.to_string();
    format!(
        r#"{{"type":"message","id":"x","message":{{"role":"{role}",{}"#,
        body.replacen('{', "", 1) + "}"
    )
}

fn call_line(args: &serde_json::Value) -> String {
    message_line(
        "assistant",
        json!({"content": [{"type":"toolCall","id":"c","name":"write","arguments":args}]}),
    )
}

fn write_lines(dir: &Path, lines: &[String]) -> PathBuf {
    let p = dir.join("t.jsonl");
    fs::write(&p, format!("{}\n", lines.join("\n"))).unwrap();
    p
}

#[test]
fn small_args_stay_structured_and_images_get_stable_ids() {
    let items = read_all(&rich());
    assert_eq!(items.len(), 3);
    let TranscriptItem::User { text, images, .. } = &items[0] else {
        panic!("{:?}", items[0]);
    };
    assert_eq!(text, "look at this\n[image]", "text keeps its old shape");
    assert_eq!(
        images,
        &[ImageRef {
            blob: PNG_ID.into(),
            mime: "image/png".into(),
            bytes: 8
        }]
    );
    let TranscriptItem::ToolCall { args_value, .. } = &items[1] else {
        panic!("{:?}", items[1]);
    };
    assert_eq!(
        args_value,
        &Some(json!({"path": "src/a.rs", "sel": "L1-L3"}))
    );
    let TranscriptItem::ToolResult {
        details, images, ..
    } = &items[2]
    else {
        panic!("{:?}", items[2]);
    };
    assert_eq!(
        details,
        &Some(json!({"lines": 3, "meta": {"truncated": false}}))
    );
    assert_eq!(
        images,
        &[ImageRef {
            blob: JPG_ID.into(),
            mime: "image/jpeg".into(),
            bytes: 6
        }]
    );
}

#[test]
fn long_string_leaves_are_cut_and_oversize_args_dropped() {
    let d = tempfile::tempdir().unwrap();
    let long = "é".repeat(20_000); // 40 000 bytes, multi-byte on purpose
    let big = "x".repeat(16 * 1024);
    let p = write_lines(
        d.path(),
        &[
            call_line(&json!({"content": long, "n": [1, {"deep": long}]})),
            call_line(&json!({"a": big, "b": big, "c": big, "d": big, "e": big})),
        ],
    );
    let items = read_all(&p);
    let TranscriptItem::ToolCall {
        args, args_value, ..
    } = &items[0]
    else {
        panic!()
    };
    assert!(
        args.len() <= 4 * 1024,
        "the pretty text keeps its 4 KiB cap"
    );
    let v = args_value.as_ref().unwrap();
    for s in [
        v["content"].as_str().unwrap(),
        v["n"][1]["deep"].as_str().unwrap(),
    ] {
        assert!(s.ends_with('…') && s.len() <= 16 * 1024 + '…'.len_utf8());
        assert!(s.starts_with("éé"));
    }
    assert_eq!(v["n"][0], 1);
    let TranscriptItem::ToolCall { args_value, .. } = &items[1] else {
        panic!()
    };
    assert_eq!(args_value, &None, "5 × 16 KiB is over 64 KiB");
}

#[test]
fn oversize_details_are_dropped() {
    let d = tempfile::tempdir().unwrap();
    let result = |details: serde_json::Value| {
        message_line(
            "toolResult",
            json!({"toolCallId":"c","toolName":"bash","content":"out","details":details}),
        )
    };
    let p = write_lines(
        d.path(),
        &[
            result(json!({"ok": "y".repeat(30 * 1024)})),
            result(json!({"ok": "y".repeat(33 * 1024)})),
        ],
    );
    let items = read_all(&p);
    let details = |i: usize| match &items[i] {
        TranscriptItem::ToolResult { details, .. } => details.clone(),
        other => panic!("{other:?}"),
    };
    assert!(details(0).is_some());
    assert_eq!(details(1), None);
}

#[test]
fn read_blob_finds_decodes_and_tolerates_a_truncated_tail() {
    let (mime, bytes) = read_blob(&rich(), PNG_ID, 1 << 20).unwrap().unwrap();
    assert_eq!((mime.as_str(), bytes), ("image/png", PNG_BYTES.to_vec()));
    let (mime, bytes) = read_blob(&rich(), JPG_ID, 1 << 20).unwrap().unwrap();
    assert_eq!(
        (mime.as_str(), bytes),
        ("image/jpeg", vec![0, 1, 2, 3, 4, 5])
    );
    assert_eq!(
        read_blob(&rich(), "0000000000000000", 1 << 20).unwrap(),
        None
    );
    assert_eq!(
        read_blob(&rich(), PNG_ID, 4).unwrap(),
        None,
        "over the caller's cap"
    );

    let d = tempfile::tempdir().unwrap();
    let mut text = fs::read_to_string(rich()).unwrap();
    // A half-written image line without its newline.
    text.push_str(
        r#"{"type":"message","message":{"role":"user","content":[{"type":"image","data":"AAAA"#,
    );
    let p = d.path().join("cut.jsonl");
    fs::write(&p, text).unwrap();
    assert!(read_blob(&p, PNG_ID, 1 << 20).unwrap().is_some());
    assert_eq!(read_blob(&p, "0000000000000000", 1 << 20).unwrap(), None);
    // The reader itself also survives it.
    assert_eq!(read_all(&p).len(), 3);
    assert!(read_blob(&d.path().join("nope.jsonl"), PNG_ID, 1).is_err());
}

#[test]
fn read_blob_resolves_externalized_images_from_the_blob_store() {
    let d = tempfile::tempdir().unwrap();
    let hash = "ab".repeat(32);
    let data = format!("blob:sha256:{hash}");
    let sessions = d.path().join("agent/sessions/proj");
    fs::create_dir_all(&sessions).unwrap();
    fs::create_dir_all(d.path().join("agent/blobs")).unwrap();
    let line = message_line(
        "user",
        json!({"content":[{"type":"image","data":data,"mimeType":"image/png"}]}),
    );
    let p = sessions.join("s.jsonl");
    fs::write(&p, format!("{line}\n")).unwrap();

    let items = read_all(&p);
    let TranscriptItem::User { images, .. } = &items[0] else {
        panic!()
    };
    assert_eq!(images.len(), 1);
    assert_eq!(images[0].bytes, 0, "size unknown without reading the store");
    let id = images[0].blob.clone();

    assert_eq!(read_blob(&p, &id, 1 << 20).unwrap(), None, "blob gc'd");
    fs::write(d.path().join("agent/blobs").join(&hash), PNG_BYTES).unwrap();
    assert_eq!(
        read_blob(&p, &id, 1 << 20).unwrap(),
        Some(("image/png".into(), PNG_BYTES.to_vec()))
    );
}
