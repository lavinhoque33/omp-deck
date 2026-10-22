//! `CtlClient` against an in-test fake extension server on a unix socket.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use deck_core::ctl::{CtlClient, CtlCommand, CtlReply, LiveModel, LiveSnapshot, LiveStatus};

const HELLO: &str = r#"{"t":"hello","v":1,"pid":4242}"#;

fn snapshot_line(pid: u32, agents: &str) -> String {
    let agents = agents.replace('\n', "");
    format!(
        r#"{{"t":"agents","pid":{pid},"sessionFile":"/s/main.jsonl","mainIdle":true,"leaf":"e7","agents":[{agents}]}}"#
    )
}

const AGENTS: &str = r#"
 {"id":"0-A","name":"Alpha","kind":"sub","parent":null,"status":"running","activity":"running bash","live":true,"streaming":true,"createdAt":1,"lastActivity":2,"sessionFile":"/s/main/0-A.jsonl","model":"m1"},
 {"id":"1-B","name":"Beta","kind":"sub","parent":"0-A","status":"parked","activity":null,"live":false,"streaming":null,"sessionFile":"/s/main/1-B.jsonl","model":null},
 {"id":"2-C","status":"nonsense"},
 {"id":"3-D","name":"Dead","status":"aborted","live":false}"#;

struct Fake {
    listener: UnixListener,
}

fn fake(dir: &Path) -> Fake {
    let path = dir.join("ctl.sock");
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path).unwrap();
    Fake { listener }
}

impl Fake {
    fn accept(&self) -> UnixStream {
        let (s, _) = self.listener.accept().unwrap();
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        s
    }
}

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

fn say(s: &mut UnixStream, line: &str) {
    s.write_all(line.as_bytes()).unwrap();
    s.write_all(b"\n").unwrap();
}

fn snapshot(c: &CtlClient) -> Arc<LiveSnapshot> {
    wait_for("snapshot", || c.latest().is_some());
    c.latest().unwrap()
}

#[test]
fn parses_hello_and_snapshot_including_parked() {
    let tmp = tempfile::tempdir().unwrap();
    let srv = fake(tmp.path());
    let updates = Arc::new(AtomicUsize::new(0));
    let u = Arc::clone(&updates);
    let client = CtlClient::spawn(tmp.path().to_path_buf(), move || {
        u.fetch_add(1, Ordering::SeqCst);
    });
    assert!(client.latest().is_none());
    let mut s = srv.accept();
    say(&mut s, HELLO);
    say(&mut s, &snapshot_line(4242, AGENTS));

    let snap = snapshot(&client);
    assert!(client.connected());
    assert_eq!(snap.pid, 4242);
    assert_eq!(
        snap.session_file.as_deref(),
        Some(Path::new("/s/main.jsonl"))
    );
    assert_eq!(snap.main_idle, Some(true));
    assert_eq!(snap.leaf.as_deref(), Some("e7"));
    // the malformed "2-C" entry is skipped, the rest survive
    let ids: Vec<_> = snap.agents.iter().map(|a| a.id.as_str()).collect();
    assert_eq!(ids, ["0-A", "1-B", "3-D"]);
    let a = &snap.agents[0];
    assert_eq!(a.status, LiveStatus::Running);
    assert_eq!(a.activity.as_deref(), Some("running bash"));
    assert_eq!(a.streaming, Some(true));
    assert!(a.live);
    assert_eq!(a.model.as_deref(), Some("m1"));
    let b = &snap.agents[1];
    assert_eq!(b.status, LiveStatus::Parked);
    assert_eq!(b.parent.as_deref(), Some("0-A"));
    assert!(!b.live);
    assert_eq!(
        b.session_file.as_deref(),
        Some(Path::new("/s/main/1-B.jsonl"))
    );
    assert_eq!(snap.agents[2].status, LiveStatus::Aborted);
    assert_eq!(snap.agents[2].name, "Dead");

    // an identical re-push is not an update; a changed one is
    let before = updates.load(Ordering::SeqCst);
    say(&mut s, &snapshot_line(4242, AGENTS));
    say(&mut s, &snapshot_line(4242, ""));
    wait_for("empty snapshot", || {
        client.latest().is_some_and(|s| s.agents.is_empty())
    });
    assert_eq!(updates.load(Ordering::SeqCst), before + 1);
}

#[test]
fn reconnects_after_server_restart() {
    let tmp = tempfile::tempdir().unwrap();
    let srv = fake(tmp.path());
    let client = CtlClient::spawn(tmp.path().to_path_buf(), || {});
    let mut s = srv.accept();
    say(&mut s, HELLO);
    say(&mut s, &snapshot_line(1, AGENTS));
    assert_eq!(snapshot(&client).pid, 1);

    drop(s);
    drop(srv); // omp /restart: the socket vanishes
    wait_for("disconnect", || !client.connected());
    assert!(client.latest().is_none());

    let srv = fake(tmp.path());
    let mut s = srv.accept();
    say(&mut s, HELLO);
    say(&mut s, &snapshot_line(2, ""));
    wait_for("reconnect", || client.latest().is_some_and(|s| s.pid == 2));
    assert!(client.connected());
}

#[test]
fn commands_map_to_reply_ids() {
    let tmp = tempfile::tempdir().unwrap();
    let srv = fake(tmp.path());
    let client = CtlClient::spawn(tmp.path().to_path_buf(), || {});
    let mut s = srv.accept();
    say(&mut s, HELLO);
    say(&mut s, &snapshot_line(1, AGENTS));
    snapshot(&client);

    let kill = client.send(CtlCommand::Kill {
        id: "0-A".into(),
        root: "/s/main.jsonl".into(),
    });
    let chat = client.send(CtlCommand::Chat {
        id: "1-B".into(),
        text: "hi \"there\"\nline2".into(),
        root: "/s/main.jsonl".into(),
    });
    let revive = client.send(CtlCommand::Revive {
        id: "nope".into(),
        root: "/s/main.jsonl".into(),
    });
    assert!(kill != chat && chat != revive && kill != revive);

    let mut rd = BufReader::new(s.try_clone().unwrap());
    let mut got = Vec::new();
    for _ in 0..3 {
        let mut l = String::new();
        rd.read_line(&mut l).unwrap();
        got.push(serde_json::from_str::<serde_json::Value>(&l).unwrap());
    }
    assert_eq!(got[0]["t"], "kill");
    assert_eq!(got[0]["id"], "0-A");
    assert_eq!(got[0]["rid"], kill);
    for g in &got {
        assert_eq!(
            g["root"], "/s/main.jsonl",
            "every command carries the expected root"
        );
    }
    assert_eq!(got[1]["t"], "chat");
    assert_eq!(got[1]["text"], "hi \"there\"\nline2");
    assert_eq!(got[1]["rid"], chat);
    assert_eq!(got[2]["t"], "revive");
    assert_eq!(got[2]["rid"], revive);

    // answered out of order, with an unknown rid thrown in
    say(
        &mut s,
        &format!(r#"{{"t":"reply","rid":{revive},"ok":false,"error":"no such subagent"}}"#),
    );
    say(&mut s, r#"{"t":"reply","rid":99999,"ok":true}"#);
    say(
        &mut s,
        &format!(r#"{{"t":"reply","rid":{kill},"ok":true,"released":true}}"#),
    );
    say(
        &mut s,
        &format!(r#"{{"t":"reply","rid":{chat},"ok":true}}"#),
    );

    let mut replies = Vec::new();
    wait_for("3 replies", || {
        replies.extend(client.take_replies());
        replies.len() >= 3
    });
    replies.sort_by_key(|(id, _)| *id);
    assert_eq!(
        replies,
        vec![
            (
                kill,
                CtlReply {
                    ok: true,
                    error: None,
                    new_file: None,
                    leaf: None,
                }
            ),
            (
                chat,
                CtlReply {
                    ok: true,
                    error: None,
                    new_file: None,
                    leaf: None,
                }
            ),
            (
                revive,
                CtlReply {
                    ok: false,
                    error: Some("no such subagent".into()),
                    new_file: None,
                    leaf: None,
                }
            ),
        ]
    );
    assert!(client.take_replies().is_empty());
}

#[test]
fn fork_at_and_navigate_round_trip() {
    let tmp = tempfile::tempdir().unwrap();
    let srv = fake(tmp.path());
    let client = CtlClient::spawn(tmp.path().to_path_buf(), || {});
    let mut s = srv.accept();
    say(&mut s, HELLO);
    say(&mut s, &snapshot_line(1, ""));
    snapshot(&client);

    let fork = client.send(CtlCommand::ForkAt {
        root: "/s/main.jsonl".into(),
        entry_id: "a1".into(),
    });
    let nav = client.send(CtlCommand::Navigate {
        root: "/s/main.jsonl".into(),
        entry_id: "u2".into(),
    });
    let mut rd = BufReader::new(s.try_clone().unwrap());
    let mut got = Vec::new();
    for _ in 0..2 {
        let mut l = String::new();
        rd.read_line(&mut l).unwrap();
        got.push(serde_json::from_str::<serde_json::Value>(&l).unwrap());
    }
    assert_eq!(got[0]["t"], "fork_at");
    assert_eq!(got[0]["entryId"], "a1");
    assert_eq!(got[0]["root"], "/s/main.jsonl");
    assert_eq!(got[0]["rid"], fork);
    assert_eq!(got[1]["t"], "navigate");
    assert_eq!(got[1]["entryId"], "u2");
    assert_eq!(got[1]["rid"], nav);

    say(
        &mut s,
        &format!(r#"{{"t":"reply","rid":{fork},"ok":true,"newFile":"/s/new.jsonl"}}"#),
    );
    say(
        &mut s,
        &format!(r#"{{"t":"reply","rid":{nav},"ok":false,"error":"busy"}}"#),
    );
    let mut replies = Vec::new();
    wait_for("2 replies", || {
        replies.extend(client.take_replies());
        replies.len() >= 2
    });
    replies.sort_by_key(|(id, _)| *id);
    assert_eq!(
        replies[0].1.new_file.as_deref(),
        Some(Path::new("/s/new.jsonl"))
    );
    assert!(replies[0].1.ok);
    assert_eq!(replies[1].1.error.as_deref(), Some("busy"));
}

#[test]
fn unanswered_requests_fail_on_disconnect_and_when_offline() {
    let tmp = tempfile::tempdir().unwrap();
    let srv = fake(tmp.path());
    let client = CtlClient::spawn(tmp.path().to_path_buf(), || {});
    let mut s = srv.accept();
    say(&mut s, HELLO);
    say(&mut s, &snapshot_line(1, AGENTS));
    snapshot(&client);

    let pending = client.send(CtlCommand::Kill {
        id: "0-A".into(),
        root: "/s/main.jsonl".into(),
    });
    let mut l = String::new();
    BufReader::new(s.try_clone().unwrap())
        .read_line(&mut l)
        .unwrap();
    drop(s);
    drop(srv);
    let mut replies = Vec::new();
    wait_for("failure reply", || {
        replies.extend(client.take_replies());
        !replies.is_empty()
    });
    assert_eq!(replies[0].0, pending);
    assert!(!replies[0].1.ok);

    wait_for("offline", || !client.connected());
    let offline = client.send(CtlCommand::Revive {
        id: "x".into(),
        root: "/s/main.jsonl".into(),
    });
    wait_for("offline reply", || {
        replies.extend(client.take_replies());
        replies.iter().any(|(id, r)| *id == offline && !r.ok)
    });
}

#[test]
fn oversized_and_malformed_lines_are_ignored() {
    let tmp = tempfile::tempdir().unwrap();
    let srv = fake(tmp.path());
    let client = CtlClient::spawn(tmp.path().to_path_buf(), || {});
    let mut s = srv.accept();
    say(&mut s, HELLO);
    say(&mut s, "not json at all");
    say(&mut s, r#"{"t":"mystery","x":1}"#);
    say(&mut s, r#"{"t":"agents","pid":"wrong type"}"#);
    // 9 MiB line (over the 8 MiB bound), then a valid snapshot on the same connection
    let big = vec![b'x'; 9 * 1024 * 1024];
    s.write_all(&big).unwrap();
    s.write_all(b"\n").unwrap();
    // a huge line that never terminates before the next real one, split across writes
    s.write_all(&big).unwrap();
    s.write_all(b"tail\n").unwrap();
    say(&mut s, &snapshot_line(7, AGENTS));

    let snap = snapshot(&client);
    assert_eq!(snap.pid, 7);
    assert_eq!(snap.agents.len(), 3);
    assert!(client.connected());
}

#[test]
fn drop_is_prompt_connected_or_not() {
    let tmp = tempfile::tempdir().unwrap();

    // no server at all (backing off)
    let client = CtlClient::spawn(tmp.path().to_path_buf(), || {});
    std::thread::sleep(Duration::from_millis(250));
    let t = Instant::now();
    drop(client);
    assert!(t.elapsed() < Duration::from_secs(1), "{:?}", t.elapsed());

    // silent server holding the connection open
    let srv = fake(tmp.path());
    let client = CtlClient::spawn(tmp.path().to_path_buf(), || {});
    let mut s = srv.accept();
    say(&mut s, HELLO);
    wait_for("connected", || client.connected());
    let t = Instant::now();
    drop(client);
    assert!(t.elapsed() < Duration::from_secs(1), "{:?}", t.elapsed());
}

#[test]
fn context_breakdown_is_parsed_and_absent_or_malformed_is_none() {
    let tmp = tempfile::tempdir().unwrap();
    let srv = fake(tmp.path());
    let client = CtlClient::spawn(tmp.path().to_path_buf(), || {});
    let mut s = srv.accept();
    say(&mut s, HELLO);
    let with = |ctx: &str| {
        let ctx = ctx.replace('\n', "");
        format!(r#"{{"t":"agents","pid":9,"agents":[],"context":{ctx}}}"#)
    };

    say(
        &mut s,
        &with(
            r#"{"model":"Fake One","contextWindow":50000,"usedTokens":1234,"anchored":true,
                "categories":[{"id":"systemPrompt","label":"System prompt","tokens":2901},
                              {"id":"messages","tokens":0},{"label":"no id","tokens":1}],
                "autoCompactBuffer":16384,"freeTokens":32382,"thresholdTokens":33616,
                "snapcompactSaved":null,
                "tools":[{"name":"task","tokens":881},{"name":"eval","tokens":653},{"oops":1}],
                "skills":3,"computedAt":1790000000000,"futureField":{"x":1}}"#,
        ),
    );
    wait_for("context snapshot", || {
        client.latest().is_some_and(|s| s.context.is_some())
    });
    let ctx = client.latest().unwrap().context.clone().unwrap();
    assert_eq!(ctx.model.as_deref(), Some("Fake One"));
    assert_eq!(
        (ctx.context_window, ctx.used_tokens, ctx.anchored),
        (50000, 1234, true)
    );
    let cats: Vec<_> = ctx
        .categories
        .iter()
        .map(|c| (c.id.as_str(), c.label.as_str(), c.tokens))
        .collect();
    assert_eq!(
        cats,
        [
            ("systemPrompt", "System prompt", 2901),
            ("messages", "messages", 0)
        ],
        "label defaults to the id; a category without an id is skipped"
    );
    assert_eq!(
        (
            ctx.auto_compact_buffer,
            ctx.free_tokens,
            ctx.threshold_tokens,
            ctx.snapcompact_saved
        ),
        (16384, 32382, Some(33616), None)
    );
    assert_eq!(
        ctx.tools,
        [("task".to_string(), 881), ("eval".to_string(), 653)]
    );
    assert_eq!((ctx.skills, ctx.computed_at_ms), (3, 1_790_000_000_000));

    // threshold null (auto-compaction off) stays None
    say(
        &mut s,
        &with(r#"{"contextWindow":1000,"usedTokens":10,"thresholdTokens":null}"#),
    );
    wait_for("minimal context", || {
        client
            .latest()
            .and_then(|s| s.context.as_ref().map(|c| c.context_window))
            == Some(1000)
    });
    assert_eq!(
        client
            .latest()
            .unwrap()
            .context
            .as_ref()
            .unwrap()
            .threshold_tokens,
        None
    );

    // missing required numbers → no context, but the snapshot itself survives
    say(&mut s, &with(r#"{"model":"x"}"#));
    wait_for("malformed context", || {
        client.latest().is_some_and(|s| s.context.is_none())
    });
    assert_eq!(client.latest().unwrap().pid, 9);

    // an older extension sends no field at all; `null` means unavailable
    say(&mut s, &with(r#"{"contextWindow":5,"usedTokens":1}"#));
    wait_for("context again", || {
        client.latest().is_some_and(|s| s.context.is_some())
    });
    say(&mut s, &snapshot_line(9, ""));
    wait_for("older extension", || {
        client.latest().is_some_and(|s| s.context.is_none())
    });
    say(&mut s, &with("null"));
    say(&mut s, &with(r#"{"contextWindow":5,"usedTokens":1}"#));
    wait_for("context after null", || {
        client.latest().is_some_and(|s| s.context.is_some())
    });
    say(&mut s, &with("null"));
    wait_for("null context", || {
        client.latest().is_some_and(|s| s.context.is_none())
    });
}

#[test]
fn main_model_state_is_parsed_and_absent_or_malformed_is_none() {
    let tmp = tempfile::tempdir().unwrap();
    let srv = fake(tmp.path());
    let client = CtlClient::spawn(tmp.path().to_path_buf(), || {});
    let mut s = srv.accept();
    say(&mut s, HELLO);
    let with = |m: &str| format!(r#"{{"t":"agents","pid":9,"agents":[],"model":{m}}}"#);
    let model = || client.latest().and_then(|s| s.model.clone());

    say(
        &mut s,
        &with(
            r#"{"selector":"anthropic/claude-sonnet-5-5","thinkingLevel":"high","configured":"high"}"#,
        ),
    );
    wait_for("model state", || model().is_some());
    assert_eq!(
        model(),
        Some(LiveModel {
            selector: "anthropic/claude-sonnet-5-5".into(),
            thinking: "high".into(),
            thinking_configured: Some("high".into()),
        })
    );

    // No effective level is "off", as in the file's `thinking_level_change`.
    say(
        &mut s,
        &with(r#"{"selector":"p/m","thinkingLevel":null,"configured":"auto"}"#),
    );
    wait_for("level off", || model().is_some_and(|m| m.selector == "p/m"));
    let m = model().unwrap();
    assert_eq!(m.thinking, "off");
    assert_eq!(m.thinking_configured.as_deref(), Some("auto"));

    // malformed or empty → no model, but the snapshot itself survives
    say(&mut s, &with(r#"{"thinkingLevel":"high"}"#));
    wait_for("malformed model", || {
        client.latest().is_some_and(|s| s.model.is_none())
    });
    assert_eq!(client.latest().unwrap().pid, 9);
    say(&mut s, &with(r#"{"selector":"p/m"}"#));
    wait_for("model again", || model().is_some());
    say(&mut s, &with(r#"{"selector":""}"#));
    wait_for("empty selector", || model().is_none());

    // an older extension sends no field at all
    say(&mut s, &with(r#"{"selector":"p/m"}"#));
    wait_for("model again", || model().is_some());
    say(&mut s, &snapshot_line(9, ""));
    wait_for("older extension", || model().is_none());
}
