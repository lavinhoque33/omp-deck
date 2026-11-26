//! The Wave 21 ctl commands against a real omp driven by a scripted model, launched through the
//! session host as a TSP terminal (the chat view's way), with the sessions helper alongside.
//!
//! Runs only when `OMPDECK_TEST_OMP=<path to omp>` is set (sandboxed HOME / `PI_*` / `XDG_*`; no
//! real model, no network beyond loopback). The `omp-deck-host` binary is built when missing.

mod support;

use std::collections::HashMap;
use std::time::Duration;

use deck_core::ctl::{BranchSummary, CtlCommand, NavigateOutcome};
use deck_core::omp::{AdvisorScope, SessError};
use deck_tsp::wire::Event;
use serde_json::{Value, json};
use support::fake_model::{self as fm, Reply};
use support::{Live, TURN, wait_for};

const SUMMARY: &str = "Fake branch summary";

/// Answers a conversation turn with `Reply to <prompt>` and every side request (summaries, titles:
/// no tools offered) with [`SUMMARY`].
fn script(request: &Value) -> Reply {
    if !fm::has_tools(request) {
        return Reply::text(SUMMARY);
    }
    let prompt = prompt_of(request);
    if prompt == "slow" {
        return Reply::slow_text("Reply to slow", Duration::from_secs(6));
    }
    Reply::text(&format!("Reply to {prompt}"))
}

/// The user's own text: omp prefixes the first message of a turn with a system reminder.
fn prompt_of(request: &Value) -> String {
    fm::last_user_text(request)
        .lines()
        .last()
        .unwrap_or_default()
        .to_owned()
}

fn message_ids(entries: &[Value], role: &str) -> Vec<String> {
    entries
        .iter()
        .filter(|e| e["type"] == "message" && e["message"]["role"] == role)
        .filter_map(|e| e["id"].as_str().map(str::to_owned))
        .collect()
}

/// Submits `prompt` and waits until its reply is persisted and omp is idle.
fn turn(live: &Live, prompt: &str) {
    let want = message_ids(&live.entries(), "assistant").len() + 1;
    live.send(prompt);
    wait_for("the reply", TURN, || {
        let entries = live.entries();
        let leaf = entries.iter().rev().find_map(|e| e["id"].as_str());
        // A fast scripted reply can finish before ctl's debounced busy snapshot. Do not
        // mistake the pre-turn idle snapshot for the completed turn's state.
        (message_ids(&entries, "assistant").len() >= want
            && live
                .client()
                .latest()
                .is_some_and(|s| s.main_idle == Some(true) && s.leaf.as_deref() == leaf))
        .then_some(())
    });
}

fn nav(live: &Live, id: &str, summary: BranchSummary) -> deck_core::ctl::CtlReply {
    live.ctl(
        CtlCommand::Navigate {
            root: live.session_file(),
            entry_id: id.to_owned(),
            summary,
            take_editor: false,
        },
        TURN,
    )
}

#[test]
fn navigate_summarises_the_branch_left_and_redraws_the_tab() {
    let Some(live) = Live::start("nav", script, |_| {}, |_| None) else {
        eprintln!("skipping: OMPDECK_TEST_OMP is not set");
        return;
    };
    turn(&live, "one");
    turn(&live, "two");
    let rows = live.entries();
    let (assistants, users) = (message_ids(&rows, "assistant"), message_ids(&rows, "user"));
    assert_eq!((assistants.len(), users.len()), (2, 2));
    wait_for("the tab to show the second answer", TURN, || {
        live.peek()
            .to_string()
            .contains("Reply to two")
            .then_some(())
    });

    // Plain move: no summary, the tab is redrawn by omp's own command context.
    let plain = nav(&live, &assistants[0], BranchSummary::None);
    assert!(plain.ok, "{plain:?}");
    assert_eq!(plain.leaf.as_deref(), Some(assistants[0].as_str()));
    assert!(
        plain.tail.as_ref().is_some_and(|tail| {
            tail != &assistants[0]
                && live
                    .entries()
                    .iter()
                    .any(|entry| entry["id"] == tail.as_str())
        }),
        "{plain:?}",
    );
    wait_for("the tab to drop the second answer", TURN, || {
        (!live.peek().to_string().contains("Reply to two")).then_some(())
    });
    assert!(live.entries().iter().all(|e| e["type"] != "branch_summary"));

    // Already there: omp's no-op, reported as its message.
    let same = nav(&live, &assistants[0], BranchSummary::None);
    assert!(same.ok);
    assert_eq!(same.message.as_deref(), Some("Already at this point"));
    assert_eq!(same.tail, plain.tail);

    // With a summary: the branch left (u1 a1 → back at a1's sibling work) is summarised by the
    // model into a `branch_summary` entry on the new leaf's path.
    turn(&live, "three");
    let before = live.model.count();
    let reply = nav(
        &live,
        &assistants[0],
        BranchSummary::Custom("focus on perf".into()),
    );
    assert!(reply.ok, "{reply:?}");
    let summaries: Vec<Value> = live
        .entries()
        .into_iter()
        .filter(|e| e["type"] == "branch_summary")
        .collect();
    assert_eq!(summaries.len(), 1, "{summaries:?}");
    assert!(
        summaries[0]["summary"]
            .as_str()
            .is_some_and(|t| t.ends_with(SUMMARY)),
        "{summaries:?}"
    );
    assert_eq!(summaries[0]["parentId"], assistants[0].as_str());
    assert_eq!(
        reply.leaf.as_deref(),
        summaries[0]["id"].as_str(),
        "the summary is the new leaf"
    );
    assert_eq!(reply.tail, reply.leaf, "the summary was just appended");
    let request = live
        .model
        .find(before, |r| {
            fm::messages(r)
                .iter()
                .any(|m| fm::text(m).contains("focus on perf"))
        })
        .expect("the custom instructions reached the model");
    assert!(!fm::has_tools(&request), "the summary is a side request");
    wait_for("the summary on screen", TURN, || {
        let tab = live.peek().to_string();
        (tab.contains(SUMMARY) && !tab.contains("Reply to three")).then_some(())
    });

    // Navigating while omp works is refused, never queued.
    live.send("slow");
    wait_for("omp to be streaming", TURN, || {
        (!live.main_idle()).then_some(())
    });
    let refused = nav(&live, &assistants[0], BranchSummary::None);
    assert_eq!(
        (refused.ok, refused.error.as_deref()),
        (false, Some("busy"))
    );
}

#[cfg(unix)]
fn overlapping_navigations(live: &Live, id: &str) -> deck_core::ctl::CtlReply {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;

    let dir = deck_core::ctl::ctl_dir(&live.ns, support::SESSION);
    let mut socket = UnixStream::connect(dir.join("ctl.sock")).unwrap();
    socket.set_read_timeout(Some(TURN)).unwrap();
    let navigation = |rid| {
        json!({
            "t": "navigate", "rid": rid, "root": live.session_file(), "entryId": id, "takeEditor": true,
        })
    };
    // Write both requests in one small socket write: the extension dispatches both before
    // the first navigation can resume from its await.
    let batch = format!("{}\n{}\n", navigation(1), navigation(2));
    socket.write_all(batch.as_bytes()).unwrap();
    let mut replies = [None, None];
    for line in BufReader::new(socket).lines() {
        let reply: Value = serde_json::from_str(&line.unwrap()).unwrap();
        if reply["t"] == "reply" {
            let rid = reply["rid"].as_u64().unwrap() as usize;
            replies[rid - 1] =
                Some(serde_json::from_value::<deck_core::ctl::CtlReply>(reply).unwrap());
            if replies.iter().all(Option::is_some) {
                break;
            }
        }
    }
    let [first, second] = replies;
    let second = second.unwrap();
    assert_eq!((second.ok, second.error.as_deref()), (false, Some("busy")));
    first.unwrap()
}

fn composer_pty_input(live: &Live, bytes: &[u8]) {
    let namespace = deck_host::Namespace::new(&live.ns).unwrap();
    let attached = deck_host::attach(
        &namespace,
        support::SESSION,
        deck_host::AttachParams {
            cols: 100,
            rows: 30,
            cell_width: 8,
            cell_height: 16,
            history_lines: 0,
            palette: deck_host::Palette::default(),
        },
    )
    .unwrap();
    let (mut rx, tx) = attached.split();
    tx.input(bytes).unwrap();
    tx.detach().unwrap();
    while let Ok(event) = rx.recv() {
        if matches!(
            event,
            deck_host::HostEvent::Closed | deck_host::HostEvent::Exited(_)
        ) {
            break;
        }
    }
}

const PNG: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+jL9sAAAAASUVORK5CYII=";

#[test]
fn navigate_takes_a_prompt_and_its_images_without_clobbering_existing_text() {
    let Some(exe) = std::env::var_os("OMPDECK_TEST_OMP") else {
        eprintln!("skipping: OMPDECK_TEST_OMP is not set");
        return;
    };
    let version = std::process::Command::new(exe)
        .arg("--version")
        .output()
        .unwrap();
    if !String::from_utf8_lossy(&version.stdout).starts_with("omp/19.") {
        eprintln!("skipping: the composer clear-draft path uses omp 19");
        return;
    }
    let live = Live::start(
        "nav-take-editor",
        script,
        |_| {},
        |live| {
            let file = live.proj.join("fixture.jsonl");
            let header = json!({
                "type": "session", "version": 3,
                "id": "01a2a687-1000-7000-8000-e034503e0001",
                "timestamp": "2026-11-26T10:00:00Z", "cwd": live.proj,
            });
            let before = json!({
                "type": "custom", "id": "before", "parentId": null,
                "timestamp": "2026-11-26T10:00:01Z", "customType": "seed", "data": {},
            });
            let oversized_prompt = json!({
                "type": "message", "id": "oversized", "parentId": "before",
                "timestamp": "2026-11-26T10:00:02Z",
                "message": {"role": "user", "timestamp": 1795687202000_u64,
                    "content": "é".repeat(32 * 1024 + 1)},
            });
            let prompt = json!({
                "type": "message", "id": "prompt", "parentId": "before",
                "timestamp": "2026-11-26T10:00:02Z",
                "message": {"role": "user", "timestamp": 1795687202000_u64, "content": [
                    {"type": "text", "text": "prior prompt"},
                    {"type": "image", "mimeType": "image/png", "data": PNG},
                ]},
            });
            std::fs::write(
                &file,
                format!("{header}\n{before}\n{oversized_prompt}\n{prompt}\n"),
            )
            .unwrap();
            Some(file)
        },
    )
    .unwrap();
    wait_for("snapshot advertising the persisted prompt", TURN, || {
        live.client()
            .latest()
            .filter(|s| s.tail.as_deref() == Some("prompt"))
    });
    let navigate = |entry_id: &str| {
        live.ctl(
            CtlCommand::Navigate {
                root: live.session_file(),
                entry_id: entry_id.into(),
                summary: BranchSummary::None,
                take_editor: true,
            },
            TURN,
        )
    };
    let check_outcome = |reply: deck_core::ctl::CtlReply| {
        assert!(reply.ok, "{reply:?}");
        assert_eq!(reply.leaf.as_deref(), Some("before"));
        assert_eq!(
            NavigateOutcome::of(&reply),
            NavigateOutcome {
                editor_text: Some("prior prompt".into()),
                editor_images: 1
            },
        );
    };
    #[cfg(unix)]
    check_outcome(overlapping_navigations(&live, "prompt"));

    let restored = navigate("prompt");
    assert_eq!(restored.tail.as_deref(), Some("prompt"));
    check_outcome(restored);
    assert_eq!(
        live.ctl(CtlCommand::EditorState, TURN).editor_empty(),
        Some(true)
    );
    turn(&live, "fresh prompt");
    let entries = live.entries();
    let new_message = entries
        .iter()
        .rfind(|e| e["message"]["role"] == "user")
        .unwrap();
    assert_ne!(new_message["id"], "prompt");
    assert!(
        new_message["message"]["content"]
            .as_array()
            .is_none_or(|parts| parts.iter().all(|p| p["type"] != "image"))
    );

    let (sf, id) = wait_for("ready composer", TURN, || live.composer());
    let draft = "keep this draft";
    live.events(&[Event::Edit {
        sf,
        id,
        from: 0,
        to: 0,
        text: draft.into(),
        cursor: draft.len() as u32,
        len: 0,
    }]);
    assert_eq!(
        live.ctl(CtlCommand::EditorState, TURN).editor_empty(),
        Some(false)
    );
    check_outcome(navigate("prompt"));
    wait_for("untouched draft", TURN, || {
        live.peek().to_string().contains(draft).then_some(())
    });

    let (sf, id) = wait_for("ready composer", TURN, || live.composer());
    live.events(&[Event::Edit {
        sf,
        id,
        from: 0,
        to: draft.len() as u32,
        text: String::new(),
        cursor: 0,
        len: draft.len() as u32,
    }]);
    assert_eq!(
        live.ctl(CtlCommand::EditorState, TURN).editor_empty(),
        Some(true)
    );

    // Paste an actual image path into omp and submit via Enter. TSP Send would replace
    // the draft and lose the attachment rather than expanding the native composer chip.
    let image = live.proj.join("pixel.png");
    use base64::Engine as _;
    std::fs::write(
        &image,
        base64::engine::general_purpose::STANDARD
            .decode(PNG)
            .unwrap(),
    )
    .unwrap();
    composer_pty_input(
        &live,
        format!("\x1b[200~{}\x1b[201~", image.display()).as_bytes(),
    );
    wait_for("omp's pasted image chip", Duration::from_secs(12), || {
        live.peek().to_string().contains("#1").then_some(())
    });
    let native_prompt = "attached prompt (literal [Image #9])";
    composer_pty_input(
        &live,
        format!("\x1b[200~{native_prompt}\x1b[201~\r").as_bytes(),
    );
    let native_id = wait_for(
        "the submitted image prompt and completed turn",
        TURN,
        || {
            let entries = live.entries();
            let snapshot = live.client().latest()?;
            if snapshot.main_idle != Some(true)
                || snapshot.leaf.as_deref() != entries.iter().rev().find_map(|e| e["id"].as_str())
            {
                return None;
            }
            let user = entries.iter().rfind(|e| e["message"]["role"] == "user")?;
            let content = user["message"]["content"].as_array()?;
            let text = content.iter().find_map(|p| p["text"].as_str())?;
            if !text.contains(native_prompt) {
                return None;
            }
            assert!(
                text.starts_with("[Image #1"),
                "expected the native marker in {text:?}"
            );
            assert_eq!(content.iter().filter(|p| p["type"] == "image").count(), 1);
            user["id"].as_str().map(str::to_owned)
        },
    );
    let native = navigate(&native_id);
    assert!(native.ok, "{native:?}");
    assert_eq!(
        NavigateOutcome::of(&native),
        NavigateOutcome {
            editor_text: Some(native_prompt.into()),
            editor_images: 1,
        },
    );
    assert_eq!(
        live.ctl(CtlCommand::EditorState, TURN).editor_empty(),
        Some(true)
    );

    let oversized = navigate("oversized");
    assert!(oversized.ok, "{oversized:?}");
    assert!(
        oversized.value.is_none(),
        "an oversized prompt must not be truncated for the phone"
    );
    assert_eq!(
        live.ctl(CtlCommand::EditorState, TURN).editor_empty(),
        Some(false),
        "leave the whole prompt in omp's editor",
    );
}

/// Aborting with no summary in flight is a no-op success: the leaf is unchanged and no
/// `branch_summary` entry appears. (A mid-flight cancel — Navigate+Summarize aborted while the
/// model streams the summary — is intentionally not tested here: the harness cannot hit that
/// window deterministically. A 10 s-paced summary completed before the abort landed; request
/// counting as a start signal is polluted by unrelated side requests such as title
/// generation; and an abort that lands before summarization starts is a silent no-op, so a
/// pass would prove nothing about cancellation.)
#[test]
fn abort_branch_summary_without_a_summary_is_a_noop() {
    let Some(live) = Live::start("abort", script, |_| {}, |_| None) else {
        eprintln!("skipping: OMPDECK_TEST_OMP is not set");
        return;
    };
    turn(&live, "one");
    let leaf_before = live.client().latest().and_then(|s| s.leaf.clone());
    let file = live.session_file();
    let abort = live.ctl(
        CtlCommand::AbortBranchSummary { root: file.clone() },
        Duration::from_secs(30),
    );
    assert!(abort.ok, "{abort:?}");
    assert_eq!(
        live.client().latest().and_then(|s| s.leaf.clone()),
        leaf_before
    );
    assert!(live.entries().iter().all(|e| e["type"] != "branch_summary"));
    // A stale root is refused like every other tab-bound command.
    let stale = live.ctl(
        CtlCommand::AbortBranchSummary {
            root: "/some/other.jsonl".into(),
        },
        Duration::from_secs(30),
    );
    assert_eq!(
        (stale.ok, stale.error.as_deref()),
        (false, Some("session changed"))
    );
}

#[test]
fn labels_advisors_and_the_task_model_apply_to_the_running_session() {
    let Some(live) = Live::start(
        "cfg",
        script,
        |live| {
            // Advisors are off by default: with the setting off omp stores a saved roster for
            // the next enable and reports 0 active.
            let config = live.agent.join("config.yml");
            let text = std::fs::read_to_string(&config).unwrap();
            std::fs::write(config, text + "advisor:\n  enabled: true\n").unwrap();
        },
        |_| None,
    ) else {
        eprintln!("skipping: OMPDECK_TEST_OMP is not set");
        return;
    };
    turn(&live, "one");
    let rows = live.entries();
    let user = message_ids(&rows, "user")[0].clone();
    let file = live.session_file();

    // Labels: appended to the live session in any state; removal appends an entry without one.
    let label = |text: Option<&str>| {
        live.ctl(
            CtlCommand::SetLabel {
                root: file.clone(),
                entry_id: user.clone(),
                label: text.map(str::to_owned),
            },
            TURN,
        )
    };
    assert!(label(Some("  start here  ")).ok);
    let last_label = || {
        live.entries()
            .into_iter()
            .rev()
            .find(|e| e["type"] == "label")
            .map(|e| (e["targetId"].clone(), e["label"].clone()))
    };
    wait_for("the label on disk", TURN, || {
        (last_label() == Some((json!(user), json!("start here")))).then_some(())
    });
    assert!(label(None).ok);
    wait_for("the label removal on disk", TURN, || {
        (last_label() == Some((json!(user), Value::Null))).then_some(())
    });
    let unknown = live.ctl(
        CtlCommand::SetLabel {
            root: file.clone(),
            entry_id: "nope".into(),
            label: Some("x".into()),
        },
        TURN,
    );
    assert_eq!(
        (unknown.ok, unknown.error.as_deref()),
        (false, Some("no such entry"))
    );
    let stale = live.ctl(
        CtlCommand::SetLabel {
            root: "/some/other.jsonl".into(),
            entry_id: user.clone(),
            label: None,
        },
        TURN,
    );
    assert_eq!(stale.error.as_deref(), Some("session changed"));

    // A session a live omp owns is refused by the closed-session helpers, nothing written.
    let size = std::fs::metadata(&file).unwrap().len();
    assert!(matches!(
        live.omp.rename_session(&file, "Taken"),
        Err(SessError::Busy)
    ));
    assert!(matches!(
        live.omp.set_label(&file, &user, Some("x")),
        Err(SessError::Busy)
    ));
    assert_eq!(std::fs::metadata(&file).unwrap().len(), size);

    // Advisors: saved to the project's WATCHDOG.yml, then applied to this session.
    let advisor = |doc: Option<Value>| {
        live.ctl(
            CtlCommand::AdvisorConfig {
                root: Some(file.clone()),
                scope: AdvisorScope::Project,
                doc,
            },
            TURN,
        )
    };
    let read = advisor(None);
    assert!(read.ok, "{read:?}");
    let watchdog = live.proj.join("WATCHDOG.yml");
    assert_eq!(
        read.value.as_ref().unwrap()["path"],
        watchdog.to_string_lossy().as_ref()
    );
    assert_eq!(read.value.as_ref().unwrap()["doc"]["advisors"], json!([]));

    let doc = json!({
        "instructions": "Keep notes short.",
        "futureTop": {"k": 1},
        "advisors": [{"name": "Reviewer", "model": "fake/fake-1", "tools": ["read"],
            "reviewMode": "turn", "syncBacklog": "3", "futureKnob": ["x"]}],
    });
    let saved = advisor(Some(doc));
    assert!(saved.ok, "{saved:?}");
    assert_eq!(
        saved.message.as_deref(),
        Some("Saved project WATCHDOG.yml — 1 advisor active.")
    );
    assert!(watchdog.is_file());
    let saved_doc = &saved.value.as_ref().unwrap()["doc"];
    assert_eq!(saved_doc["advisors"][0]["name"], "Reviewer");
    assert_eq!(saved_doc["advisors"][0]["syncBacklog"], "3");
    let reread = advisor(None);
    assert_eq!(
        reread.value.as_ref().unwrap()["doc"]["instructions"],
        "Keep notes short."
    );

    let bad = advisor(Some(
        json!({"advisors": [{"name": "X", "reviewMode": "sometimes"}]}),
    ));
    assert!(!bad.ok, "an invalid entry is refused, not written: {bad:?}");
    assert!(
        std::fs::read_to_string(&watchdog)
            .unwrap()
            .contains("Reviewer")
    );

    let cleared = advisor(Some(json!({"advisors": []})));
    assert_eq!(
        cleared.message.as_deref(),
        Some("Removed project WATCHDOG.yml — 0 advisors active.")
    );
    assert!(!watchdog.exists());

    // The Task subagent model: session-only; clearing returns to what is configured (nothing).
    let task = |model: Option<&str>| {
        live.ctl(
            CtlCommand::TaskModelOverride {
                root: Some(file.clone()),
                model: model.map(str::to_owned),
            },
            TURN,
        )
    };
    let set = task(Some("fake/fake-1"));
    assert!(set.ok, "{set:?}");
    assert_eq!(set.value, Some(json!("fake/fake-1")));
    let off = task(None);
    assert_eq!(off.value, None, "nothing configured: no override remains");
    assert!(!task(Some("not a model")).ok);
    assert!(
        !std::fs::read_to_string(live.agent.join("config.yml"))
            .unwrap()
            .contains("agentModelOverrides"),
        "the override is never persisted"
    );
}

/// The ask the tab currently shows: `(surface, option list id, action bar id)`. omp 18.18's
/// form ask: the question's options are the `omp.ask.options` list keyed `f<field>` (the outer
/// one of that role, keyed `body<tab>`, is the form's scroller).
fn ask_dock(live: &Live) -> Option<(String, String, String)> {
    let doc = live.peek();
    let nodes = support::nodes(&doc);
    let list = nodes.iter().find(|(id, k, p)| {
        k == "list"
            && p["role"] == "omp.ask.options"
            && id
                .rsplit('/')
                .next()
                .is_some_and(|key| key.starts_with('f'))
    })?;
    let actions = nodes
        .iter()
        .find(|(_, k, p)| k == "row" && p["role"] == "omp.actions")?;
    let sf = doc["surfaces"]
        .as_array()?
        .iter()
        .find(|s| s["open"] == true)?["id"]
        .as_str()?;
    Some((sf.to_owned(), list.0.clone(), actions.0.clone()))
}

/// Answers the shown ask: picks option `index` and submits, or cancels the dialog (`None`).
/// The submit waits until the tab shows the pick: selecting re-renders the card, and until omp has
/// described it again its action buttons resolve to the disposed nodes, so a submit sent in that
/// window is dropped ("TSP: event for an unknown node") and the dialog stays open.
fn answer_ask(live: &Live, index: Option<usize>) {
    let (sf, list, actions) = wait_for("omp's ask dialog in the tab", TURN, || ask_dock(live));
    let action = |act: &str| Event::Action {
        sf: sf.clone(),
        id: format!("{actions}/{act}"),
        act: act.to_owned(),
        value: None,
        mods: None,
    };
    let Some(i) = index else {
        live.events(&[action("cancel")]);
        return;
    };
    let item = format!("{list}/o{i}");
    live.events(&[Event::Select {
        sf: sf.clone(),
        id: list.clone(),
        item: item.clone(),
    }]);
    wait_for("the picked option in the tab", TURN, || {
        support::nodes(&live.peek())
            .iter()
            .any(|(id, _, p)| *id == list && p["selected"] == item.as_str())
            .then_some(())
    });
    live.events(&[action("submit")]);
}

/// The model sees the ask's answer as a tool result and echoes it.
fn ask_script(request: &Value) -> Reply {
    if !fm::has_tools(request) {
        return Reply::text(SUMMARY);
    }
    let tail = fm::after_last_user(request);
    if let Some(result) = tail.iter().rev().find(|m| fm::role(m) == "tool") {
        return Reply::text(&format!("Answered: {}", fm::text(result)));
    }
    if prompt_of(request) == "ASKME" {
        return Reply::tool_call(
            "ask",
            &json!({"html": "<form><p>Pick one</p>\
                <label><input type=\"radio\" name=\"pick\" value=\"alpha\"> alpha</label>\
                <label><input type=\"radio\" name=\"pick\" value=\"beta\"> beta</label></form>"}),
        );
    }
    Reply::text("unscripted")
}

#[test]
fn ui_answers_are_fenced_to_the_omp_process_incarnation() {
    let Some(live) = Live::start("ask-pid", ask_script, |_| {}, |_| None) else {
        eprintln!("skipping: OMPDECK_TEST_OMP is not set");
        return;
    };
    live.send("ASKME");
    let snapshot = wait_for("the pending ask dialog", TURN, || {
        live.client()
            .latest()
            .filter(|snapshot| !snapshot.pending_ui.is_empty())
    });
    assert!(live.client().ui_hook());
    let id = snapshot.pending_ui[0].id;
    let root = snapshot
        .session_file
        .clone()
        .expect("the ask's conversation root");
    let before = live.model.count();
    let refused = live.ctl(
        CtlCommand::AnswerUi {
            id,
            value: json!({"fields":[{"checked":[0],"other":null}]}),
            root: root.clone(),
            pid: Some(snapshot.pid.wrapping_add(1)),
        },
        TURN,
    );
    assert!(!refused.ok, "{refused:?}");
    assert_eq!(refused.error.as_deref(), Some("no such request"));
    assert!(
        live.client()
            .latest()
            .unwrap()
            .pending_ui
            .iter()
            .any(|pending| pending.id == id)
    );
    assert!(
        live.entries()
            .iter()
            .all(|entry| entry["message"]["toolName"] != "ask"),
        "the stale process answer must leave the ask unresolved"
    );

    let accepted = live.ctl(
        CtlCommand::AnswerUi {
            id,
            value: json!({"fields":[{"checked":[1],"other":null}]}),
            root,
            pid: Some(snapshot.pid),
        },
        TURN,
    );
    assert!(accepted.ok, "{accepted:?}");
    wait_for(
        "the correct process answer in the next model step",
        TURN,
        || {
            live.model.find(before, |request| {
                fm::has_tools(request)
                    && fm::after_last_user(request).iter().any(|message| {
                        fm::role(message) == "tool"
                            && fm::text(message).contains("beta")
                            && !fm::text(message).contains("alpha")
                    })
            })
        },
    );
    wait_for("the answered dialog to close", TURN, || {
        live.client()
            .latest()
            .filter(|snapshot| snapshot.pending_ui.iter().all(|pending| pending.id != id))
    });
}

#[test]
fn reanswering_an_ask_opens_omps_dialog_and_resumes_the_turn() {
    let Some(live) = Live::start("ask", ask_script, |_| {}, |_| None) else {
        eprintln!("skipping: OMPDECK_TEST_OMP is not set");
        return;
    };
    live.send("ASKME");
    answer_ask(&live, Some(1));
    wait_for("the first answer's reply", TURN, || {
        (message_ids(&live.entries(), "assistant").len() >= 2 && live.main_idle()).then_some(())
    });
    let ask_results = |rows: &[Value]| -> Vec<Value> {
        rows.iter()
            .filter(|e| e["type"] == "message" && e["message"]["toolName"] == "ask")
            .cloned()
            .collect()
    };
    let rows = live.entries();
    let first = ask_results(&rows);
    assert_eq!(first.len(), 1);
    let first_text = first[0]["message"]["content"].to_string();
    assert!(first_text.contains("beta"), "{first_text}");
    let file = live.session_file();
    let target = first[0]["id"].as_str().unwrap().to_owned();
    let reask = |id: &str| {
        live.client().send(CtlCommand::ReanswerAsk {
            root: file.clone(),
            entry_id: id.to_owned(),
        })
    };
    let reply_of = |rid: u64| {
        wait_for("the re-answer's reply", TURN, || {
            live.client()
                .take_replies()
                .into_iter()
                .find(|(r, _)| *r == rid)
                .map(|(_, r)| r)
        })
    };

    // Only an ask's result can be re-answered.
    let user = message_ids(&rows, "user")[0].clone();
    let refused = reply_of(reask(&user));
    assert_eq!(
        (refused.ok, refused.error.as_deref()),
        (false, Some("not an ask result"))
    );

    // Skipping omp's dialog leaves the session untouched: the leaf stays on its branch (omp may
    // still append bookkeeping such as the turn's `model_usage` below it).
    let leaf = live.client().latest().and_then(|s| s.leaf.clone()).unwrap();
    let cancelled = reask(&target);
    answer_ask(&live, None);
    let cancelled = reply_of(cancelled);
    assert_eq!(
        (cancelled.ok, cancelled.error.as_deref()),
        (false, Some("cancelled"))
    );
    // The leaf first: every entry it names is in the file read after it.
    let now = live.client().latest().and_then(|s| s.leaf.clone()).unwrap();
    let rows = live.entries();
    assert_eq!(ask_results(&rows).len(), 1, "no answer was recorded");
    let parent: HashMap<&str, &str> = rows
        .iter()
        .filter_map(|e| Some((e["id"].as_str()?, e["parentId"].as_str()?)))
        .collect();
    assert!(
        std::iter::successors(Some(now.as_str()), |id| parent.get(id).copied())
            .any(|id| id == leaf),
        "the leaf {now} left the branch of {leaf}"
    );

    // Answering: a sibling result on the same tool call, and the turn resumes with it.
    let before = live.model.count();
    let rid = reask(&target);
    answer_ask(&live, Some(0));
    let done = reply_of(rid);
    assert!(done.ok, "{done:?}");
    let all = ask_results(&live.entries());
    assert_eq!(all.len(), 2, "the old answer stays as a sibling branch");
    assert_eq!(done.leaf.as_deref(), all[1]["id"].as_str());
    assert_eq!(
        done.tail, done.leaf,
        "report the journal before resuming the turn"
    );
    assert_eq!(all[0]["parentId"], all[1]["parentId"]);
    assert_eq!(
        all[0]["message"]["toolCallId"],
        all[1]["message"]["toolCallId"]
    );
    let new_text = all[1]["message"]["content"].to_string();
    assert!(
        new_text.contains("alpha") && !new_text.contains("beta"),
        "{new_text}"
    );
    // The resumed turn's own reply, not just any model call (omp's side requests count too).
    wait_for("the resumed turn's reply", TURN, || {
        let last = live
            .entries()
            .into_iter()
            .rev()
            .find(|e| e["type"] == "message" && e["message"]["role"] == "assistant")?;
        (last["message"]["content"].to_string().contains("alpha") && live.main_idle()).then_some(())
    });
    let resumed = live
        .model
        .find(before, |r| {
            fm::has_tools(r)
                && fm::after_last_user(r)
                    .iter()
                    .any(|m| fm::text(m).contains("alpha"))
        })
        .expect("the model received the new answer");
    assert!(
        !fm::after_last_user(&resumed)
            .iter()
            .any(|m| fm::text(m).contains("beta"))
    );
}

/// Two more models next to `fake-1`: a thinking-capable one and one with a tiny context window.
fn two_more_models(live: &Live) {
    let yml = live.model.models_yml().replace(
        "        input: [text, image]\n",
        "        input: [text, image]\n      - id: fake-think\n        name: Fake Think\n        reasoning: true\n        \
         contextWindow: 200000\n        maxTokens: 4096\n        input: [text]\n      - id: fake-small\n        \
         name: Fake Small\n        contextWindow: 8\n        maxTokens: 4\n        input: [text]\n",
    );
    std::fs::write(live.agent.join("models.yml"), yml).unwrap();
    let config = live.agent.join("config.yml");
    let text = std::fs::read_to_string(&config).unwrap();
    std::fs::write(config, text + "  slow: fake/fake-think\n").unwrap();
}

/// `(role, model)` of every `model_change` and the level of every `thinking_level_change`.
fn model_and_thinking_entries(rows: &[Value]) -> (Vec<(String, String)>, Vec<String>) {
    let models = rows
        .iter()
        .filter(|e| e["type"] == "model_change")
        .map(|e| {
            (
                e["role"].as_str().unwrap_or("").to_owned(),
                e["model"].as_str().unwrap_or("").to_owned(),
            )
        })
        .collect();
    let levels = rows
        .iter()
        .filter(|e| e["type"] == "thinking_level_change")
        .map(|e| e["thinkingLevel"].as_str().unwrap_or("").to_owned())
        .collect();
    (models, levels)
}

fn snapshot_model(live: &Live) -> (String, String) {
    let m = live
        .client()
        .latest()
        .and_then(|s| s.model.clone())
        .expect("a model in the snapshot");
    (m.selector, m.thinking)
}

/// Before the first reply omp may report no context usage; its TUI `/switch` counts that as 0 tokens
/// (`getContextUsage()?.tokens ?? 0`), so a fresh session switches directly instead of falling back.
#[test]
fn switch_model_before_the_first_reply_switches_directly() {
    let Some(live) = Live::start("sw0", script, two_more_models, |_| None) else {
        eprintln!("skipping: OMPDECK_TEST_OMP is not set");
        return;
    };
    let r = live.ctl(
        CtlCommand::SwitchModel {
            selector: "fake/fake-think".to_owned(),
        },
        TURN,
    );
    assert!(r.ok, "{r:?}");
    wait_for("the snapshot to show the model", TURN, || {
        (snapshot_model(&live).0 == "fake/fake-think").then_some(())
    });
}

#[test]
fn switch_model_and_set_effort_match_the_typed_commands() {
    let Some(live) = Live::start("sw", script, two_more_models, |_| None) else {
        eprintln!("skipping: OMPDECK_TEST_OMP is not set");
        return;
    };
    turn(&live, "one");
    // An omp upgrade that breaks a route internal answers "unsupported" (the gateway then falls back to the typed
    // commands): a current omp must never do so, in any case below.
    let supported = |r: deck_core::ctl::CtlReply| {
        assert_ne!(
            r.error.as_deref(),
            Some("unsupported"),
            "omp's internals drifted: {r:?}"
        );
        r
    };
    let switch = |sel: &str| {
        supported(live.ctl(
            CtlCommand::SwitchModel {
                selector: sel.to_owned(),
            },
            TURN,
        ))
    };
    let effort = |lvl: &str| {
        supported(live.ctl(
            CtlCommand::SetEffort {
                level: lvl.to_owned(),
            },
            TURN,
        ))
    };
    let in_snapshot = |want: &str| {
        wait_for("the snapshot to show the model", TURN, || {
            (snapshot_model(&live).0 == want).then_some(())
        });
    };

    // By provider/id, by @role, with a :level suffix.
    let by_id = switch("fake/fake-think");
    assert!(by_id.ok, "{by_id:?}");
    assert_eq!(by_id.value.as_ref().unwrap()["model"], "fake/fake-think");
    assert_eq!(
        by_id.message.as_deref(),
        Some("Session-only model: fake/fake-think")
    );
    in_snapshot("fake/fake-think");
    let back = switch("fake/fake-1");
    assert!(back.ok, "{back:?}");
    in_snapshot("fake/fake-1");
    let by_role = switch("@slow");
    assert!(by_role.ok, "{by_role:?}");
    in_snapshot("fake/fake-think");
    let with_level = switch("fake/fake-think:high");
    assert!(with_level.ok, "{with_level:?}");
    assert_eq!(
        snapshot_model(&live),
        ("fake/fake-think".to_owned(), "high".to_owned())
    );
    let (models, levels) = model_and_thinking_entries(&live.entries());
    assert!(
        models.len() >= 4 && models.iter().skip(1).all(|(role, _)| role == "temporary"),
        "{models:?}"
    );
    assert_eq!(levels.last().map(String::as_str), Some("high"));

    // Unknown selector.
    let unknown = switch("nope/none");
    assert_eq!(
        (unknown.ok, unknown.error.as_deref()),
        (false, Some("Unknown model: nope/none"))
    );

    // Effort: a valid level is written, a bad one is omp's text.
    let ok = effort("low");
    assert!(ok.ok, "{ok:?}");
    assert_eq!(ok.message.as_deref(), Some("Thinking set to low."));
    assert_eq!(snapshot_model(&live).1, "low");
    assert_eq!(
        model_and_thinking_entries(&live.entries())
            .1
            .last()
            .map(String::as_str),
        Some("low")
    );
    let bad = effort("bogus");
    let err = bad.error.unwrap_or_default();
    assert!(
        !bad.ok && err.starts_with("Unknown thinking level: bogus. Available:"),
        "{err}"
    );

    // Identity with the typed commands: the same entries (roles, models, levels).
    let before = model_and_thinking_entries(&live.entries());
    live.send("/switch fake/fake-1");
    wait_for("the typed /switch", TURN, || {
        (model_and_thinking_entries(&live.entries()).0.len() > before.0.len()).then_some(())
    });
    let typed = model_and_thinking_entries(&live.entries());
    assert_eq!(
        typed.0.last().unwrap(),
        &("temporary".to_owned(), "fake/fake-1".to_owned())
    );
    live.send("/switch fake/fake-think");
    wait_for("the typed /switch back", TURN, || {
        (snapshot_model(&live).0 == "fake/fake-think").then_some(())
    });
    let n = model_and_thinking_entries(&live.entries()).1.len();
    live.send("/effort medium");
    wait_for("the typed /effort", TURN, || {
        (model_and_thinking_entries(&live.entries()).1.len() > n).then_some(())
    });
    let typed_levels = model_and_thinking_entries(&live.entries()).1;
    assert_eq!(typed_levels.last().map(String::as_str), Some("medium"));
    let ctl_medium = effort("high");
    assert!(ctl_medium.ok);
    let after = model_and_thinking_entries(&live.entries()).1;
    assert_eq!(
        after.last().map(String::as_str),
        Some("high"),
        "same entry kind as the typed /effort"
    );

    // Over context: the target's window is smaller than the transcript -> nothing changes.
    turn(&live, "fill the window");
    let before = (
        snapshot_model(&live),
        model_and_thinking_entries(&live.entries()),
    );
    let over = switch("fake/fake-small");
    assert_eq!(
        (over.ok, over.error.as_deref()),
        (false, Some("needs_compact")),
        "{over:?}"
    );
    assert_eq!(
        (
            snapshot_model(&live),
            model_and_thinking_entries(&live.entries())
        ),
        before
    );
}
