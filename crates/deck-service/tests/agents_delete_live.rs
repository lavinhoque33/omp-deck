//! `CtlCommand::DeleteAgents` against a real omp (18.15+) driven by the scripted model, through the
//! control extension: the parent's `task` calls spawn a finished subagent, one that itself spawns a
//! (finished) subagent, and one whose model reply is held so it stays running.
//!
//! - `keep_running: true` deletes the finished ones (files and nested directory gone, absent from the
//!   next live snapshot) and skips the running one (`running`, still listed, file present); targets
//!   outside the root's artifact directory, an advisor transcript and a symlinked nested directory
//!   are refused or unlinked without touching anything outside.
//! - `keep_running: false` then stops the running one through omp: its files go, the parent's turn
//!   ends normally, and the session still answers.
//!
//! Private HOME/XDG tree and own `omp-deck-test-*` namespace; skipped when omp is not installed.
#![cfg(unix)]

mod support;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use deck_core::ctl::{
    CtlClient, CtlCommand, CtlReply, DeleteOutcome, DeleteTarget, LiveAgent, LiveSnapshot,
    LiveStatus, ctl_dir, ctl_env, install_extension, prepare_ctl_dir,
};
use deck_host::{AttachParams, HostEvent, Namespace, Palette, SessionSpec};
use deck_tsp::terminal::{InputEncoding, encode_event};
use deck_tsp::wire::Event;
use serde_json::{Value, json};
use support::device::{OMP_CONFIG, Sandbox};
use support::fake_model::{self as fm, FakeModel, Reply};

const SESSION: &str = "agdel1";
const TURN: Duration = Duration::from_secs(90);
/// The held subagent's reply: far longer than the test, cut off when omp aborts the request.
const HOLD: Duration = Duration::from_secs(600);

const MARK: &str = "DEL-MARK";
const PARENT_DONE: &str = "All helpers handled.";
const RESULT: &str = "Helper result: done.";

/// The text of the assignment that tells the scripted subagents apart.
const TASK_A: &str = "DEL-A report that you are done";
const TASK_N: &str = "DEL-N spawn one helper of your own";
const TASK_G: &str = "DEL-G report that you are done";
const TASK_B: &str = "DEL-B wait for the go-ahead";

fn task_call(name: &str, task: &str) -> Reply {
    Reply::tool_call(
        "task",
        &json!({
            "name": name,
            "agent": "task",
            "task": task,
            "solutionSpace": "one fix: reply, nothing to research",
        }),
    )
}

/// Every user message of the conversation, joined: omp's background-job notices arrive as later
/// user messages, so the assignment is not always the newest one.
fn user_text(req: &Value) -> String {
    fm::messages(req)
        .iter()
        .filter(|m| fm::role(m) == "user")
        .map(fm::text)
        .collect::<Vec<_>>()
        .join("\n")
}

/// `task` calls the conversation's own model already made.
fn task_calls(req: &Value) -> usize {
    fm::messages(req)
        .iter()
        .filter(|m| fm::role(m) == "assistant")
        .flat_map(|m| m["tool_calls"].as_array().into_iter().flatten())
        .filter(|c| c["function"]["name"] == "task")
        .count()
}

/// The parent starts the three helpers one after the other (omp runs tasks in the background and
/// answers each call at once) and then just answers whatever notice comes in. The subagents are
/// told apart by their assignment.
fn script() -> impl FnMut(&Value) -> Reply + Send + 'static {
    move |req| {
        if !fm::has_tools(req) {
            // A side request (e.g. the session title).
            return Reply::text("Fake title");
        }
        let users = user_text(req);
        if users.contains("Complete assignment thoroughly") {
            return if users.contains(TASK_B) {
                Reply::slow_text(RESULT, HOLD)
            } else if users.contains(TASK_N) && task_calls(req) == 0 {
                task_call("Grandchild", TASK_G)
            } else {
                Reply::text(RESULT)
            };
        }
        if !users.contains(MARK) {
            return Reply::text("unscripted");
        }
        match task_calls(req) {
            0 => task_call("Done", TASK_A),
            1 => task_call("Nester", TASK_N),
            2 => task_call("Held", TASK_B),
            _ => Reply::text(PARENT_DONE),
        }
    }
}

/// Kills every session of the namespace, also when the test panics.
struct Cleanup(Namespace);

impl Drop for Cleanup {
    fn drop(&mut self) {
        for name in self.0.list() {
            let _ = self.0.kill(&name);
        }
    }
}

fn which(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(program))
        .find(|p| p.is_file())
}

/// `omp-deck-host` next to the test binaries, rebuilt: a stale binary would run an older daemon.
fn ensure_host_exe() {
    let beside = PathBuf::from(support::device::GATEWAY).with_file_name("omp-deck-host");
    let status = std::process::Command::new(env!("CARGO"))
        .args(["build", "-p", "deck-host", "--bin", "omp-deck-host"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .status()
        .expect("run cargo build");
    assert!(status.success(), "cargo build -p deck-host");
    assert!(beside.is_file(), "{} exists", beside.display());
    // SAFETY: single-threaded here (no daemon, model server or client started yet).
    unsafe { std::env::set_var(deck_host::HOST_EXE_ENV, &beside) };
}

fn wait_for<T>(what: &str, timeout: Duration, mut f: impl FnMut() -> Option<T>) -> T {
    let end = Instant::now() + timeout;
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(Instant::now() < end, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn peek(ns: &Namespace) -> Value {
    deck_host::tsp_peek(ns, SESSION).expect("tsp_peek")
}

/// Every node of the document's surfaces, depth first, as `(kind, props)`.
fn nodes(doc: &Value) -> Vec<(String, Value)> {
    fn walk(n: &Value, out: &mut Vec<(String, Value)>) {
        out.push((
            n["k"].as_str().unwrap_or_default().to_owned(),
            n["p"].clone(),
        ));
        for c in n["c"].as_array().into_iter().flatten() {
            walk(c, out);
        }
    }
    let mut out = Vec::new();
    for s in doc["surfaces"].as_array().into_iter().flatten() {
        walk(&s["root"], &mut out);
    }
    out
}

/// `(surface id, editor node id)` once omp shows a sendable composer.
fn composer(doc: &Value) -> Option<(String, String)> {
    let sf = doc["surfaces"]
        .as_array()?
        .iter()
        .find(|s| s["mode"] == "inline" && s["open"] == true)?["id"]
        .as_str()?
        .to_owned();
    fn editor(n: &Value) -> Option<String> {
        if n["k"] == "editor" && n["p"]["sendable"] == true {
            return n["id"].as_str().map(str::to_owned);
        }
        n["c"].as_array()?.iter().find_map(editor)
    }
    doc["surfaces"]
        .as_array()?
        .iter()
        .find_map(|s| editor(&s["root"]))
        .map(|id| (sf, id))
}

/// Attaches, writes the `send` event for `text` and detaches again.
fn send(ns: &Namespace, sf: &str, id: &str, text: &str) {
    let params = AttachParams {
        cols: 100,
        rows: 30,
        cell_width: 8,
        cell_height: 16,
        history_lines: 0,
        palette: Palette::default(),
    };
    let attached = deck_host::attach(ns, SESSION, params).expect("attach");
    let (mut rx, tx) = attached.split();
    let reader = std::thread::spawn(move || {
        while let Ok(event) = rx.recv() {
            if matches!(event, HostEvent::Closed | HostEvent::Exited(_)) {
                break;
            }
        }
    });
    let bytes = encode_event(
        &Event::Send {
            sf: sf.to_owned(),
            id: id.to_owned(),
            text: text.to_owned(),
        },
        InputEncoding::Apc,
    );
    tx.input(&bytes).expect("send the prompt");
    tx.detach().expect("detach");
    reader.join().expect("reader thread");
}

fn snapshot(client: &CtlClient) -> Option<std::sync::Arc<LiveSnapshot>> {
    client.latest()
}

/// The live agent of that id (the `name` the parent gave the task; nested ones are `<parent>.<name>`).
fn agent_named(snap: &LiveSnapshot, id: &str) -> Option<LiveAgent> {
    snap.agents.iter().find(|a| a.id == id).cloned()
}

/// Sends `cmd` and waits for its reply.
fn call(client: &CtlClient, cmd: CtlCommand) -> CtlReply {
    let rid = client.send(cmd);
    let mut got = Vec::new();
    wait_for("the ctl reply", TURN, || {
        got.extend(client.take_replies());
        got.iter().position(|(id, _)| *id == rid)
    });
    got.into_iter()
        .find(|(id, _)| *id == rid)
        .map(|(_, r)| r)
        .expect("reply")
}

fn target(agent: &LiveAgent) -> DeleteTarget {
    DeleteTarget {
        id: agent.id.clone(),
        transcript: agent.session_file.clone().expect("a transcript"),
    }
}

fn delete(client: &CtlClient, root: &Path, agents: Vec<DeleteTarget>, keep: bool) -> DeleteOutcome {
    let reply = call(
        client,
        CtlCommand::DeleteAgents {
            root: root.to_owned(),
            agents,
            keep_running: keep,
        },
    );
    assert!(reply.ok, "delete_agents accepted: {reply:?}");
    DeleteOutcome::of(&reply).expect("an outcome")
}

fn sorted(mut v: Vec<String>) -> Vec<String> {
    v.sort();
    v
}

fn skipped(o: &DeleteOutcome) -> Vec<(String, String)> {
    let mut v: Vec<_> = o
        .skipped
        .iter()
        .map(|s| (s.id.clone(), s.reason.clone()))
        .collect();
    v.sort();
    v
}

#[test]
fn deleting_subagents_stops_and_removes_them_but_spares_running_ones_on_request() {
    let Some(omp) = which("omp") else {
        eprintln!("skipped: omp is not installed");
        return;
    };
    let config = format!("{OMP_CONFIG}modelRoles:\n  default: {}\n", fm::MODEL);
    let sb = Sandbox::new("agdel", &config);
    ensure_host_exe();
    let model = FakeModel::start(script());
    std::fs::write(sb.agent().join("models.yml"), model.models_yml()).expect("models.yml");
    let ns = Namespace::new(&sb.ns).expect("namespace");
    let _cleanup = Cleanup(Namespace::new(&sb.ns).expect("namespace"));

    // The control extension, exactly as OmpDeck launches it.
    let ctl = ctl_dir(&sb.ns, SESSION);
    prepare_ctl_dir(&ctl).expect("ctl dir");
    let ext = install_extension(&sb.root.join("ext")).expect("install the extension");
    let (mut env_set, env_remove) = deck_core::mux::host_env(SESSION, &[], false);
    env_set.retain(|(k, _)| k != "PI_NOTIFICATIONS");
    env_set.push(("PI_NOTIFICATIONS".to_owned(), "off".to_owned()));
    for (k, v) in ctl_env(&ctl).expect("ctl env") {
        env_set.push((k.to_owned(), v.to_string_lossy().into_owned()));
    }
    let spec = SessionSpec {
        name: SESSION.to_owned(),
        program: omp,
        args: vec!["-e".to_owned(), ext.display().to_string()],
        cwd: sb.proj(),
        env_set,
        env_remove,
        tsp: true,
    };
    deck_host::start(&ns, &spec, 100, 30).expect("start omp");
    let client = CtlClient::spawn(ctl, || {});
    let (sf, editor) = wait_for("a sendable composer", TURN, || composer(&peek(&ns)));
    wait_for("the control extension", TURN, || {
        (client.connected() && snapshot(&client).is_some()).then_some(())
    });

    send(&ns, &sf, &editor, &format!("{MARK} run the helpers"));

    // Done and Nester (with its Grandchild) finished; Held is running on its held reply.
    let ready = |s: &LiveSnapshot| {
        agent_named(s, "Held").is_some_and(|a| a.status == LiveStatus::Running)
            && ["Done", "Nester.Grandchild", "Nester"]
                .iter()
                .all(|n| agent_named(s, n).is_some_and(|a| a.status != LiveStatus::Running))
    };
    let end = Instant::now() + TURN;
    let snap = loop {
        if let Some(s) = snapshot(&client).filter(|s| ready(s)) {
            break s;
        }
        if Instant::now() > end {
            let seen: Vec<_> = model
                .since(0)
                .iter()
                .map(|r| {
                    (
                        fm::is_subagent(r),
                        fm::tool_names(r).join(","),
                        fm::last_user_text(r),
                    )
                })
                .collect();
            panic!(
                "timed out waiting for the helpers: agents {:#?}; model saw {seen:#?}",
                snapshot(&client).map(|s| s.agents.clone())
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let root = snap.session_file.clone().expect("the root session file");
    let artifact = root.with_extension("");
    let done = agent_named(&snap, "Done").expect("Done");
    let nester = agent_named(&snap, "Nester").expect("Nester");
    let grandchild = agent_named(&snap, "Nester.Grandchild").expect("Nester.Grandchild");
    let held = agent_named(&snap, "Held").expect("Held");
    eprintln!("agents: {:#?}", snap.agents);

    let file_of = |a: &LiveAgent| a.session_file.clone().expect("a transcript");
    for a in [&done, &nester, &held] {
        assert_eq!(
            file_of(a),
            artifact.join(format!("{}.jsonl", a.id)),
            "a top-level subagent's transcript sits in the artifact dir"
        );
        assert!(file_of(a).is_file(), "{} has a transcript", a.id);
    }
    let nested_dir = artifact.join(&nester.id);
    assert_eq!(
        file_of(&grandchild),
        nested_dir.join(format!("{}.jsonl", grandchild.id)),
        "a nested subagent lives in its parent's directory"
    );
    let done_md = artifact.join(format!("{}.md", done.id));
    assert!(done_md.is_file(), "the finished subagent's output file");

    // A stray tombstone and a nested patch next to Done; fixtures that must be refused or unlinked.
    let done_tomb = artifact.join(format!("{}.jsonl.tombstone", done.id));
    let done_patch = artifact.join(format!("{}.nested-1-x.patch", done.id));
    std::fs::write(&done_tomb, "").expect("tombstone");
    std::fs::write(&done_patch, "").expect("patch");
    // omp's structured-result sidecar.
    let done_json = artifact.join(format!("{}.json", done.id));
    std::fs::write(&done_json, "{}").expect("structured sidecar");
    let outside = sb.root.join("outside");
    std::fs::create_dir_all(&outside).expect("outside dir");
    let evil = outside.join("Evil.jsonl");
    std::fs::write(&evil, "{}\n").expect("evil transcript");
    let canary = outside.join("canary.txt");
    std::fs::write(&canary, "keep").expect("canary");
    // `<artifact>/linked` -> outside: a transcript reached through a symlinked directory.
    std::os::unix::fs::symlink(&outside, artifact.join("linked")).expect("symlinked dir");
    let evil2 = artifact.join("linked").join("Evil2.jsonl");
    std::fs::write(&evil2, "{}\n").expect("evil2 transcript");
    // `Sym` has a transcript and a nested "directory" that is a symlink to `outside`.
    let sym = artifact.join("Sym.jsonl");
    std::fs::write(&sym, "{}\n").expect("Sym transcript");
    std::os::unix::fs::symlink(&outside, artifact.join("Sym")).expect("symlinked nested dir");
    let advisor = artifact.join("__advisor-1.jsonl");
    std::fs::write(&advisor, "{}\n").expect("advisor transcript");
    // Ids that would make `<artifact>/<id>` the artifact dir itself or its parent; their transcripts
    // exist, so only the id check stops them. `keep.txt` sits in the parent.
    let dot = artifact.join("..jsonl");
    let dotdot = artifact.join("...jsonl");
    std::fs::write(&dot, "{}\n").expect("dot transcript");
    std::fs::write(&dotdot, "{}\n").expect("dotdot transcript");
    let parent = artifact.parent().expect("sessions dir").to_owned();
    let keep = parent.join("keep.txt");
    std::fs::write(&keep, "keep").expect("keep file");

    let target_of = |id: &str, file: &Path| DeleteTarget {
        id: id.to_owned(),
        transcript: file.to_owned(),
    };
    let outcome = delete(
        &client,
        &root,
        vec![
            target(&done),
            target(&nester),
            // nested under Nester: covered by the ancestor
            target(&grandchild),
            target(&held),
            target_of("Evil", &evil),
            target_of("Evil2", &evil2),
            target_of("Sym", &sym),
            target_of("__advisor-1", &advisor),
            target_of(".", &dot),
            target_of("..", &dotdot),
            // a name that does not match its file, and a path that climbs out of the artifact dir
            target_of("Other", &artifact.join("Missing.jsonl")),
            target_of("Up", &artifact.join("..").join("Up.jsonl")),
            target_of("Ghost", &artifact.join("Ghost.jsonl")),
            // the same target twice counts once
            target(&done),
        ],
        true,
    );
    eprintln!("outcome: {outcome:#?}");
    assert_eq!(
        sorted(outcome.deleted.clone()),
        sorted(vec![
            done.id.clone(),
            nester.id.clone(),
            grandchild.id.clone(),
            "Sym".to_owned()
        ])
    );
    let mut want = vec![
        (held.id.clone(), "running".to_owned()),
        ("Evil".to_owned(), "outside session".to_owned()),
        ("Evil2".to_owned(), "outside session".to_owned()),
        ("__advisor-1".to_owned(), "advisor".to_owned()),
        (".".to_owned(), "invalid target".to_owned()),
        ("..".to_owned(), "invalid target".to_owned()),
        ("Other".to_owned(), "invalid target".to_owned()),
        ("Up".to_owned(), "outside session".to_owned()),
        ("Ghost".to_owned(), "not found".to_owned()),
    ];
    want.sort();
    assert_eq!(skipped(&outcome), want);

    // Gone from disk: transcripts, output, sidecars, and the nested directory.
    for gone in [
        file_of(&done),
        done_md,
        done_tomb,
        done_patch,
        done_json,
        file_of(&nester),
        nested_dir.clone(),
        file_of(&grandchild),
        sym.clone(),
        artifact.join("Sym"),
    ] {
        assert!(
            std::fs::symlink_metadata(&gone).is_err(),
            "{} is gone",
            gone.display()
        );
    }
    // Untouched: what is outside (also behind the symlinks), the advisor transcript, the running one.
    assert!(evil.is_file() && evil2.is_file() && canary.is_file());
    assert!(advisor.is_file(), "advisor transcripts are never deleted");
    assert!(
        dot.is_file()
            && dotdot.is_file()
            && artifact.is_dir()
            && parent.is_dir()
            && root.is_file()
            && keep.is_file(),
        "`.` and `..` ids removed nothing: not the artifact dir, not its parent"
    );
    assert!(
        file_of(&held).is_file(),
        "the running subagent's transcript"
    );

    // Absent from the next live snapshot; the running one is still listed, still running.
    let after = wait_for("the snapshot without the deleted agents", TURN, || {
        let s = snapshot(&client)?;
        let gone = ["Done", "Nester", "Nester.Grandchild"]
            .iter()
            .all(|n| agent_named(&s, n).is_none());
        gone.then_some(s)
    });
    assert_eq!(
        agent_named(&after, "Held").map(|a| a.status),
        Some(LiveStatus::Running),
        "the skipped agent still runs"
    );

    // Not skipping running ones: omp stops it, then its files go.
    let parent_requests = model.count();
    let outcome = delete(&client, &root, vec![target(&held)], false);
    assert_eq!(outcome.deleted, vec![held.id.clone()]);
    assert!(outcome.skipped.is_empty(), "{outcome:?}");
    assert!(
        std::fs::symlink_metadata(file_of(&held)).is_err(),
        "the stopped subagent's transcript is gone"
    );
    wait_for("the snapshot without Held", TURN, || {
        let s = snapshot(&client)?;
        agent_named(&s, "Held").is_none().then_some(())
    });

    // The parent hears of the stopped task like of a finished one (a notice it answers) and its turn
    // ends normally.
    let notice = wait_for("the parent's notice about Held", TURN, || {
        model.find(parent_requests, |r| {
            !user_text(r).contains("Complete assignment thoroughly")
                && fm::last_user_text(r).contains(r#"<task-result id="Held""#)
        })
    });
    let text = fm::last_user_text(&notice);
    eprintln!("the parent saw for Held: {text}");
    assert!(
        !text.contains(r#"status="completed""#),
        "the deleted task is reported as stopped: {text}"
    );
    wait_for("the parent's closing text", TURN, || {
        nodes(&peek(&ns))
            .into_iter()
            .any(|(_, p)| p["text"].as_str() == Some(PARENT_DONE))
            .then_some(())
    });

    // The session still answers, and nothing the deleted agents left behind came back.
    let reply = call(
        &client,
        CtlCommand::Revive {
            id: "nope".into(),
            root: root.clone(),
        },
    );
    assert_eq!(reply.error.as_deref(), Some("no such subagent"));
    std::thread::sleep(Duration::from_secs(2));
    let end = snapshot(&client).expect("a snapshot");
    assert!(
        end.agents
            .iter()
            .all(|a| !["Done", "Nester", "Nester.Grandchild", "Held"].contains(&a.id.as_str())),
        "{:?}",
        end.agents
    );
    for id in [&done.id, &nester.id, &held.id] {
        assert!(
            !artifact.join(format!("{id}.jsonl")).exists()
                && !artifact.join(format!("{id}.md")).exists(),
            "{id}: nothing re-created after the delete"
        );
    }
    assert!(deck_host::tsp_peek(&ns, SESSION).is_ok(), "omp is still up");
}
