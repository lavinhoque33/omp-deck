use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime};

use deck_core::agents::{
    AgentStatus, AgentSummary, AgentsScanner, AgentsSnapshot, AgentsTarget, AgentsWatcher,
    TranscriptItem, TranscriptReader, TranscriptWatcher,
};

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/agents")
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

/// Writable copy of the fixture store; returns (guard, parent session path).
fn store() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    copy_dir(&fixtures(), dir.path());
    let session = dir.path().join("sess.jsonl");
    (dir, session)
}

fn find<'a>(snap: &'a AgentsSnapshot, id: &str) -> &'a AgentSummary {
    snap.agents
        .iter()
        .find(|a| a.id == id)
        .unwrap_or_else(|| panic!("no agent {id} in {:?}", ids(snap)))
}

fn ids(snap: &AgentsSnapshot) -> Vec<&str> {
    snap.agents.iter().map(|a| a.id.as_str()).collect()
}

fn append(path: &Path, bytes: &[u8]) {
    fs::OpenOptions::new()
        .append(true)
        .open(path)
        .unwrap()
        .write_all(bytes)
        .unwrap();
}

#[test]
fn scan_lists_real_agents_in_tree_order_and_skips_junk() {
    let (_d, session) = store();
    let snap = AgentsScanner::new(session.clone()).scan(true);
    assert_eq!(snap.session, session);
    // Newest first among roots, the nested child right after its parent; stub, `.bak` files and
    // artifact logs are not agents.
    assert_eq!(
        ids(&snap),
        [
            "RunningAgent",
            "RunningAgent.Helper",
            "InterruptedAgent",
            "KilledAgent",
            "FailedAgent",
            "__advisor",
            "DoneAgent",
        ]
    );
    let depths: Vec<usize> = snap.agents.iter().map(|a| a.depth).collect();
    assert_eq!(depths, [0, 1, 0, 0, 0, 0, 0]);
    assert_eq!(
        find(&snap, "RunningAgent.Helper").parent_id.as_deref(),
        Some("RunningAgent")
    );
    assert!(
        snap.agents
            .iter()
            .filter(|a| a.id != "RunningAgent.Helper")
            .all(|a| a.parent_id.is_none())
    );
}

#[test]
fn status_inference_per_file_evidence() {
    let (_d, session) = store();
    let snap = AgentsScanner::new(session).scan(true);

    let running = find(&snap, "RunningAgent");
    assert_eq!(running.status, AgentStatus::Running);
    assert_eq!(
        running.current.as_deref(),
        Some("bash · cargo test --workspace")
    );
    assert_eq!(running.model.as_deref(), Some("openai/gpt-5"));
    assert_eq!(running.task_gist, "Run the full test suite.");
    assert!(running.output_path.is_none());

    let done = find(&snap, "DoneAgent");
    assert_eq!(done.status, AgentStatus::Done);
    assert!(done.current.is_none() && done.error.is_none());

    let failed = find(&snap, "FailedAgent");
    assert_eq!(failed.status, AgentStatus::Failed);
    assert_eq!(
        failed.error.as_deref(),
        Some("429 rate limit exceeded: retry later")
    );
    assert!(failed.output_path.is_none(), "0-byte .md is not an output");

    let killed = find(&snap, "KilledAgent");
    assert_eq!(killed.status, AgentStatus::Aborted, "tombstone");
    assert_eq!(killed.error.as_deref(), Some("Interrupted by user"));

    let interrupted = find(&snap, "InterruptedAgent");
    assert_eq!(
        interrupted.status,
        AgentStatus::Interrupted,
        "session_exit signal"
    );
    assert!(interrupted.current.is_none());

    let helper = find(&snap, "RunningAgent.Helper");
    assert_eq!(helper.status, AgentStatus::Done, "stop + .md");
    assert!(
        helper
            .output_path
            .as_ref()
            .is_some_and(|p| p.ends_with("RunningAgent.Helper.md"))
    );
}

#[test]
fn dead_owner_turns_inferred_running_into_interrupted_only() {
    let (_d, session) = store();
    let snap = AgentsScanner::new(session).scan(false);
    assert_eq!(find(&snap, "RunningAgent").status, AgentStatus::Interrupted);
    assert_eq!(find(&snap, "DoneAgent").status, AgentStatus::Done);
    assert_eq!(find(&snap, "FailedAgent").status, AgentStatus::Failed);
    assert_eq!(find(&snap, "KilledAgent").status, AgentStatus::Aborted);
}

#[test]
fn summary_facts_for_a_finished_yield_agent() {
    let (_d, session) = store();
    let snap = AgentsScanner::new(session).scan(true);
    let a = find(&snap, "DoneAgent");
    assert_eq!(a.agent.as_deref(), Some("scout"));
    // Latest model_change wins over session_init.resolvedModel (which has a `:medium` suffix).
    assert_eq!(a.model.as_deref(), Some("anthropic/claude-sonnet-5-5"));
    assert_eq!(a.thinking.as_deref(), Some("medium"));
    assert_eq!(a.task_gist, "Write the parser for foo.");
    assert!(a.task.starts_with("Complete assignment thoroughly:"));
    assert!(a.task.contains("# Goal"), "full assignment is kept");
    assert_eq!((a.requests, a.tool_calls), (2, 2));
    assert_eq!(a.tokens, 230, "(input+output+cacheWrite) per request");
    assert!((a.cost - 0.05).abs() < 1e-9);
    assert_eq!(a.context_tokens, Some(450));
    assert_eq!(a.duration, Some(Duration::from_millis(12_020)));
    let created = a
        .created
        .unwrap()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap();
    assert_eq!(created.as_millis(), 1_792_576_800_000);
    assert_eq!(a.path.file_name().unwrap(), "DoneAgent.jsonl");
    assert!(
        a.output_path
            .as_ref()
            .is_some_and(|p| p.ends_with("DoneAgent.md"))
    );

    let advisor = find(&snap, "__advisor");
    assert!(advisor.advisor && !a.advisor);
    assert_eq!(advisor.model.as_deref(), Some("anthropic/claude-opus-5"));
}

#[test]
fn incremental_tail_waits_for_partial_lines_and_survives_inode_replace() {
    let (d, session) = store();
    let mut scanner = AgentsScanner::new(session);
    let path = d.path().join("sess/RunningAgent.jsonl");
    assert_eq!(
        find(&scanner.scan(true), "RunningAgent").status,
        AgentStatus::Running
    );

    // The tool result arrives in two writes; the first has no newline yet.
    let result = br#"{"type":"message","id":"t","parentId":null,"timestamp":"2026-10-21T10:05:30.000Z","message":{"role":"toolResult","toolCallId":"tu9","toolName":"bash","content":[{"type":"text","text":"ok"}],"isError":false,"timestamp":1792577130000}}"#;
    let (head, tail) = result.split_at(result.len() / 2);
    append(&path, head);
    let snap = scanner.scan(true);
    let a = find(&snap, "RunningAgent");
    assert_eq!(
        a.current.as_deref(),
        Some("bash · cargo test --workspace"),
        "half line ignored"
    );
    append(&path, tail);
    append(&path, b"\n");
    let snap = scanner.scan(true);
    let a = find(&snap, "RunningAgent");
    assert_eq!(a.status, AgentStatus::Running);
    assert!(
        a.current.is_none(),
        "tool result completed the pending call"
    );

    // The agent finishes.
    let done = br#"{"type":"message","id":"a2","parentId":null,"timestamp":"2026-10-21T10:06:00.000Z","message":{"role":"assistant","content":[{"type":"text","text":"All green"}],"provider":"openai","model":"gpt-5","usage":{"input":1,"output":1,"cacheWrite":0,"totalTokens":10,"cost":{"total":0.5}},"stopReason":"stop","timestamp":1792577160000}}
"#;
    append(&path, done);
    let snap = scanner.scan(true);
    let a = find(&snap, "RunningAgent");
    assert_eq!(a.status, AgentStatus::Done);
    assert_eq!(a.requests, 2);

    // Atomic rewrite with a different (shorter) body: counters start over, not double up.
    let replacement = d.path().join("replacement.jsonl");
    let original = fs::read_to_string(&path).unwrap();
    let first_lines: Vec<&str> = original.lines().take(7).collect();
    fs::write(&replacement, format!("{}\n", first_lines.join("\n"))).unwrap();
    fs::rename(&replacement, &path).unwrap();
    let snap = scanner.scan(true);
    let a = find(&snap, "RunningAgent");
    assert_eq!(a.status, AgentStatus::Running);
    assert_eq!(a.requests, 1);
    assert_eq!(a.current.as_deref(), Some("bash · cargo test --workspace"));

    // Disappearing transcripts leave the roster.
    fs::remove_file(&path).unwrap();
    assert!(!ids(&scanner.scan(true)).contains(&"RunningAgent"));
}

#[test]
fn malformed_and_truncated_lines_never_break_a_scan() {
    let (d, session) = store();
    let path = d.path().join("sess/FailedAgent.jsonl");
    append(
        &path,
        b"{not json}\n{\"type\":\"message\",\"message\":{\"role\":\"assistant\"\n\xff\xfe\n",
    );
    let snap = AgentsScanner::new(session).scan(true);
    assert_eq!(find(&snap, "FailedAgent").status, AgentStatus::Failed);
    assert_eq!(snap.agents.len(), 7);
}

#[test]
fn a_missing_artifact_dir_is_an_empty_snapshot() {
    let d = tempfile::tempdir().unwrap();
    let snap = AgentsScanner::new(d.path().join("nothing.jsonl")).scan(true);
    assert!(snap.agents.is_empty());
}

// ---------------------------------------------------------------------------------------------
// Transcript mapping

fn read_all(path: &Path) -> Vec<TranscriptItem> {
    TranscriptReader::new(path.to_path_buf())
        .refresh()
        .unwrap()
        .items
}

#[test]
fn transcript_maps_every_render_shape() {
    let items = read_all(&fixtures().join("sess/DoneAgent.jsonl"));
    let kinds: Vec<&str> = items
        .iter()
        .map(|i| match i {
            TranscriptItem::ModelChange { .. } => "model",
            TranscriptItem::User { .. } => "user",
            TranscriptItem::Thinking { .. } => "thinking",
            TranscriptItem::Assistant { .. } => "assistant",
            TranscriptItem::ToolCall { .. } => "call",
            TranscriptItem::ToolResult { .. } => "result",
            TranscriptItem::Exit { .. } => "exit",
            other => panic!("unexpected {other:?}"),
        })
        .collect();
    assert_eq!(
        kinds,
        [
            "model",
            "user",
            "thinking",
            "assistant",
            "call",
            "result",
            "call",
            "result",
            "exit"
        ]
    );
    assert_eq!(
        items[0],
        TranscriptItem::ModelChange {
            model: "anthropic/claude-sonnet-5-5".into()
        }
    );
    let TranscriptItem::User {
        text,
        from_agent,
        at,
    } = &items[1]
    else {
        unreachable!()
    };
    assert!(text.starts_with("Complete assignment thoroughly:") && *from_agent);
    assert_eq!(
        at.unwrap()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_millis(),
        1_792_576_800_100
    );
    // thinkingSignature is never surfaced.
    assert_eq!(
        items[2],
        TranscriptItem::Thinking {
            text: "Let me look.".into()
        }
    );
    assert_eq!(
        items[3],
        TranscriptItem::Assistant {
            text: "Reading.".into(),
            model: Some("anthropic/claude-sonnet-5-5".into()),
            at: Some(SystemTime::UNIX_EPOCH + Duration::from_millis(1_792_576_805_000)),
        }
    );
    let TranscriptItem::ToolCall {
        call_id,
        name,
        intent,
        summary,
        args,
    } = &items[4]
    else {
        unreachable!()
    };
    assert_eq!((call_id.as_str(), name.as_str()), ("tu1", "read"));
    assert_eq!(intent.as_deref(), Some("Reading foo"));
    assert_eq!(summary, "src/foo.rs");
    assert!(args.contains("\"path\"") && args.contains("src/foo.rs"));
    assert_eq!(
        items[5],
        TranscriptItem::ToolResult {
            call_id: "tu1".into(),
            name: "read".into(),
            text: "fn main() {}".into(),
            is_error: false,
            truncated: false,
        }
    );
    assert_eq!(
        items[8],
        TranscriptItem::Exit {
            reason: "dispose (normal)".into()
        }
    );
}

#[test]
fn transcript_errors_notices_compaction_and_hidden_entries() {
    let items = read_all(&fixtures().join("sess/FailedAgent.jsonl"));
    assert_eq!(
        items.last(),
        Some(&TranscriptItem::Exit {
            reason: "dispose (normal)".into()
        })
    );
    assert!(items.contains(&TranscriptItem::Error {
        message: "429 rate limit exceeded: retry later".into()
    }));
    let killed = read_all(&fixtures().join("sess/KilledAgent.jsonl"));
    assert!(killed.contains(&TranscriptItem::Error {
        message: "Interrupted by user".into()
    }));

    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("t.jsonl");
    let big = "x".repeat(40 * 1024);
    let lines = [
        r#"{"type":"custom_message","customType":"async-result","display":true,"content":"<notice>job done</notice>","details":{}}"#.to_owned(),
        r#"{"type":"custom_message","customType":"hidden","display":false,"content":"nope"}"#.to_owned(),
        r#"{"type":"compaction","id":"c","parentId":null,"timestamp":"2026-10-21T10:00:00.000Z","summary":"Goal\nstuff","tokensBefore":9000,"tokensAfter":1200}"#.to_owned(),
        r#"{"type":"message","id":"d","message":{"role":"developer","content":[{"type":"text","text":"rule injection"}]}}"#.to_owned(),
        r#"{"type":"message","id":"c","message":{"role":"custom","customType":"eval-state-context","content":"hidden","display":false}}"#.to_owned(),
        r#"{"type":"credential_pin","id":"p","provider":"anthropic","hash":"h"}"#.to_owned(),
        r#"{"type":"system-prompt-digest-or-unknown","id":"u"}"#.to_owned(),
        format!(
            r#"{{"type":"message","id":"r","message":{{"role":"toolResult","toolCallId":"z","toolName":"bash","content":[{{"type":"text","text":"{big}"}},{{"type":"image","data":"blob:sha256:ab","mimeType":"image/png"}}],"isError":true}}}}"#
        ),
        r#"{"type":"message","id":"u2","message":{"role":"user","content":[{"type":"text","text":"steer"}],"attribution":"user","timestamp":1792577000000}}"#.to_owned(),
    ];
    fs::write(&p, format!("{}\n", lines.join("\n"))).unwrap();
    let items = read_all(&p);
    assert_eq!(items.len(), 4, "{items:?}");
    assert_eq!(
        items[0],
        TranscriptItem::Notice {
            kind: "async-result".into(),
            text: "<notice>job done</notice>".into()
        }
    );
    assert_eq!(
        items[1],
        TranscriptItem::Compaction {
            tokens_before: Some(9000),
            tokens_after: Some(1200),
            summary: "Goal\nstuff".into()
        }
    );
    let TranscriptItem::ToolResult {
        text,
        is_error,
        truncated,
        ..
    } = &items[2]
    else {
        unreachable!()
    };
    assert!(*is_error && *truncated);
    assert!(text.len() <= 16 * 1024);
    assert!(
        matches!(&items[3], TranscriptItem::User { from_agent: false, text, .. } if text == "steer")
    );
}

#[test]
fn irc_blocks_become_their_own_items() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("t.jsonl");
    let lines = [
        r#"{"type":"message","id":"s","message":{"role":"user","content":[{"type":"text","text":"[Wait interrupted by message]\n<irc from=\"parent\" agent=\"Main\">\nFYI: **re-read** `a.rs`.\n</irc>"}],"attribution":"agent","steering":true}}"#,
        r#"{"type":"message","id":"t","message":{"role":"user","content":[{"type":"text","text":"before\n<irc agent=\"Sib\">\none\n</irc>\n<irc agent=\"Sib\">\ntwo\n</irc>\nafter"}],"attribution":"agent"}}"#,
        r#"{"type":"custom_message","customType":"irc:incoming","display":true,"content":"<irc>\nIncoming IRC message from agent `Main`:\n\nhello\n</irc>"}"#,
    ];
    fs::write(&p, format!("{}\n", lines.join("\n"))).unwrap();
    let irc =
        |from: Option<&str>, agent: &str, text: &str, note: Option<&str>| TranscriptItem::Irc {
            from: from.map(Into::into),
            agent: Some(agent.into()),
            text: text.into(),
            note: note.map(Into::into),
        };
    let user = |text: &str| TranscriptItem::User {
        text: text.into(),
        from_agent: true,
        at: None,
    };
    assert_eq!(
        read_all(&p),
        vec![
            irc(
                Some("parent"),
                "Main",
                "FYI: **re-read** `a.rs`.",
                Some("Wait interrupted by message")
            ),
            user("before"),
            irc(None, "Sib", "one", None),
            irc(None, "Sib", "two", None),
            user("after"),
            irc(None, "Main", "hello", None),
        ]
    );
}

#[test]
fn transcript_reader_tails_partial_lines_and_resets_on_replace() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("t.jsonl");
    let user = |t: &str| {
        format!(
            r#"{{"type":"message","id":"u","message":{{"role":"user","content":[{{"type":"text","text":"{t}"}}],"attribution":"user"}}}}"#
        )
    };
    fs::write(&p, format!("{}\n", user("one"))).unwrap();
    let mut r = TranscriptReader::new(p.clone());
    let u = r.refresh().unwrap();
    assert!(!u.reset && u.items.len() == 1);
    assert!(r.refresh().unwrap().items.is_empty());

    let second = user("two");
    let (a, b) = second.split_at(second.len() - 7);
    append(&p, a.as_bytes());
    assert!(r.refresh().unwrap().items.is_empty(), "no newline yet");
    append(&p, b.as_bytes());
    append(&p, b"\n");
    let u = r.refresh().unwrap();
    assert_eq!(u.items.len(), 1);
    assert!(matches!(&u.items[0], TranscriptItem::User { text, .. } if text == "two"));

    // Replacement that is longer than the original: only the inode tells.
    let new = d.path().join("new.jsonl");
    fs::write(
        &new,
        format!("{}\n{}\n{}\n", user("a"), user("b"), user("c")),
    )
    .unwrap();
    fs::rename(&new, &p).unwrap();
    let u = r.refresh().unwrap();
    assert!(u.reset);
    assert_eq!(u.items.len(), 3);
}

// ---------------------------------------------------------------------------------------------
// Watchers

fn wait_for<T>(mut probe: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(v) = probe() {
            return v;
        }
        assert!(Instant::now() < deadline, "timed out waiting");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn agents_watcher_publishes_changes_and_stops_on_drop() {
    let (d, session) = store();
    let updates = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&updates);
    let watcher = AgentsWatcher::spawn(move || {
        counter.fetch_add(1, Ordering::SeqCst);
    });
    assert!(watcher.latest().is_none());
    watcher.set_target(Some(AgentsTarget {
        session: session.clone(),
        owner_alive: true,
    }));
    let snap = wait_for(|| watcher.latest());
    assert_eq!(snap.session, session);
    assert_eq!(snap.agents.len(), 7);
    assert_eq!(updates.load(Ordering::SeqCst), 1);

    // Owner exit alone republishes: the Running agent becomes Interrupted.
    watcher.set_target(Some(AgentsTarget {
        session: session.clone(),
        owner_alive: false,
    }));
    let snap = wait_for(|| {
        watcher
            .latest()
            .filter(|s| find(s, "RunningAgent").status == AgentStatus::Interrupted)
    });
    assert_eq!(snap.agents.len(), 7);

    // A new agent shows up on a later rescan (≤ 3 s idle cadence).
    let nested = d.path().join("sess/Fresh.jsonl");
    fs::copy(d.path().join("sess/DoneAgent.jsonl"), nested).unwrap();
    wait_for(|| {
        watcher
            .latest()
            .filter(|s| s.agents.iter().any(|a| a.id == "Fresh"))
    });

    // A different session drops the old snapshot at once.
    watcher.set_target(Some(AgentsTarget {
        session: d.path().join("empty.jsonl"),
        owner_alive: true,
    }));
    let snap = wait_for(|| {
        watcher
            .latest()
            .filter(|s| s.session.ends_with("empty.jsonl"))
    });
    assert!(snap.agents.is_empty());
    watcher.set_target(None);
    assert!(watcher.latest().is_none());
    drop(watcher);
}

#[test]
fn transcript_watcher_follows_appends_and_bumps_revision() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("t.jsonl");
    let line = |t: &str| {
        format!(
            r#"{{"type":"message","id":"u","message":{{"role":"user","content":"{t}","attribution":"user"}}}}"#
        )
    };
    fs::write(&p, format!("{}\n", line("first"))).unwrap();
    let hits = Arc::new(AtomicUsize::new(0));
    let h = Arc::clone(&hits);
    let w = TranscriptWatcher::spawn(p.clone(), move || {
        h.fetch_add(1, Ordering::SeqCst);
    });
    let items = wait_for(|| Some(w.latest()).filter(|i| i.len() == 1));
    let rev = w.revision();
    assert!(rev >= 1);
    append(&p, format!("{}\n", line("second")).as_bytes());
    let items2 = wait_for(|| Some(w.latest()).filter(|i| i.len() == 2));
    assert!(w.revision() > rev);
    assert_eq!(items.len(), 1, "earlier snapshots are immutable");
    assert!(matches!(&items2[1], TranscriptItem::User { text, .. } if text == "second"));
    assert!(hits.load(Ordering::SeqCst) >= 2);
    drop(w);
}

#[test]
#[cfg(unix)]
fn symlinked_and_fifo_transcripts_are_ignored_without_hanging() {
    let (d, session) = store();
    let dir = d.path().join("sess");
    std::os::unix::fs::symlink(dir.join("DoneAgent.jsonl"), dir.join("Linked.jsonl")).unwrap();
    std::os::unix::fs::symlink(dir.join("DoneAgent.md"), dir.join("FailedAgent.md.link")).unwrap();
    let fifo = dir.join("Pipe.jsonl");
    assert!(
        std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap()
            .success()
    );
    // A symlinked .md is not an output either.
    fs::remove_file(dir.join("FailedAgent.md")).unwrap();
    std::os::unix::fs::symlink(dir.join("DoneAgent.md"), dir.join("FailedAgent.md")).unwrap();
    let snap = AgentsScanner::new(session).scan(true);
    assert_eq!(snap.agents.len(), 7, "{:?}", ids(&snap));
    assert!(find(&snap, "FailedAgent").output_path.is_none());
}

#[test]
fn trailing_bookkeeping_does_not_inflate_duration_or_last_activity() {
    let (d, session) = store();
    let path = d.path().join("sess/FailedAgent.jsonl");
    append(
        &path,
        concat!(
            r#"{"type":"credential_pin","id":"p","parentId":null,"timestamp":"2026-10-22T05:37:00.000Z","provider":"a","hash":"h"}"#, "\n",
            r#"{"type":"custom","customType":"session_exit","data":{"reason":"dispose","kind":"normal"},"id":"x","parentId":null,"timestamp":"2026-10-22T05:37:00.100Z"}"#, "\n",
        )
        .as_bytes(),
    );
    let snap = AgentsScanner::new(session).scan(true);
    let a = find(&snap, "FailedAgent");
    assert_eq!(a.duration, Some(Duration::from_millis(1000)));
    let last = a
        .last_activity
        .unwrap()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap();
    assert_eq!(last.as_millis(), 1_792_576_921_000);
}
