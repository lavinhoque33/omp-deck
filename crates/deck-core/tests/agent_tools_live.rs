//! Real omp extension tools, driven by a loopback scripted model in an isolated HOME.
//! Enable with `OMPDECK_TEST_OMP=<omp>`; no real model or session store is used.

mod support;

use std::time::{Duration, Instant};

use deck_core::checkpoints::{self, CheckpointKind};
use deck_core::ctl::{CtlClient, CtlCommand, GoalStatus, TodoStatus, ToolEvent, ToolRequest};
use deck_tsp::wire::Event;
use serde_json::{Value, json};
use support::fake_model::{self as fm, Reply};
use support::{Live, TURN, wait_for};

const ALL: &str = "open_session,notify,show_changes,request_review,sessions,send_prompt,wait_session,answer_question,finish_worktree,report,fan_out";
const RESULT: &str = "Posted: verified round trip";
const DELIVERED_IDLE: &str = "DELIVERED-IDLE: the child finished.";
const DELIVERED_BUSY: &str = "DELIVERED-BUSY: the child finished during this tool.";

fn script(request: &Value) -> Reply {
    if let Some(reply) = fm::task_script(request, Duration::ZERO) {
        return reply;
    }
    if !fm::has_tools(request) {
        return Reply::text("Side request");
    }
    if fm::messages(request).iter().any(|message| {
        let text = fm::text(message);
        text.contains(DELIVERED_IDLE) || text.contains(DELIVERED_BUSY)
    }) {
        return Reply::text("Acknowledged the background result.");
    }
    let user = fm::last_user_text(request);
    let prompt = user.lines().last().unwrap_or_default();
    if prompt == "checkpoint-edit" {
        let steps = fm::after_last_user(request)
            .iter()
            .filter(|m| fm::role(m) == "tool")
            .count();
        return match steps {
            0 => Reply::tool_call("read", &json!({"path":"tracked.txt"})),
            1 => Reply::tool_call(
                "write",
                &json!({"path":"tracked.txt","content":"agent changed the file\n"}),
            ),
            2 => Reply::tool_call(
                "write",
                &json!({"path":"generated.txt","content":"second mutation in the same run\n"}),
            ),
            _ => Reply::text("Checkpointed writes finished."),
        };
    }
    if prompt == "goal-progress-completion" {
        return if fm::after_last_user(request)
            .iter()
            .any(|m| fm::role(m) == "tool")
        {
            Reply::slow_text("Goal completion recorded.", Duration::from_secs(3))
        } else {
            Reply::slow_tool_call("goal", &json!({"op":"complete"}), Duration::from_secs(2))
        };
    }
    if prompt == "todo-progress" {
        let steps = fm::after_last_user(request)
            .iter()
            .filter(|m| fm::role(m) == "tool")
            .count();
        return match steps {
            0 => Reply::tool_call(
                "todo",
                &json!({"op":"init","list":[
                    {"phase":"Implement","items":["Read requirements","Implement feature"]},
                    {"phase":"Verify","items":["Run tests"]}
                ]}),
            ),
            1 => Reply::tool_call("todo", &json!({"op":"done","task":"Read requirements"})),
            2 => Reply::tool_call("todo", &json!({"op":"start","task":"Run tests"})),
            _ => Reply::text("Progress recorded."),
        };
    }
    if fm::after_last_user(request)
        .iter()
        .any(|m| fm::role(m) == "tool")
    {
        return Reply::text("Agent received the tool result.");
    }
    if let Some(call) = prompt.strip_prefix("agent-tool ") {
        let call: Value = serde_json::from_str(call).expect("scripted tool call");
        return Reply::tool_call(call["name"].as_str().unwrap(), &call["args"]);
    }
    if let Some(args) = prompt.strip_prefix("answer-question ") {
        let args: Value = serde_json::from_str(args).expect("scripted question arguments");
        return Reply::tool_call("ompdeck_answer_question", &args);
    }
    if prompt == "deliver-busy" {
        return Reply::slow_tool_call(
            "ompdeck_notify",
            &json!({"title":"Keep this tool running until the client completes it"}),
            Duration::from_secs(2),
        );
    }
    let (tool, args) = match prompt {
        "notify" => (
            "notify",
            json!({"title":"Work finished","body":"Ready for review","level":"success"}),
        ),
        "review" => (
            "request_review",
            json!({"summary":"Review the implementation","paths":["src/lib.rs"]}),
        ),
        "send-follow-up" => (
            "send_prompt",
            json!({"session":"s-1234abcd","prompt":"Continue the implementation","mode":"follow_up"}),
        ),
        "wait" => (
            "wait_session",
            json!({"session":"s-1234abcd","timeout_seconds":3600}),
        ),
        "background-wait" => (
            "wait_session",
            json!({"session":"s-1234abcd","background":true,"timeout_seconds":86400}),
        ),
        "background-no-timeout" => (
            "wait_session",
            json!({"session":"s-1234abcd","background":true}),
        ),
        "short-background-timeout" => (
            "wait_session",
            json!({"session":"s-1234abcd","background":true,"timeout_seconds":0}),
        ),
        "long-background-timeout" => (
            "wait_session",
            json!({"session":"s-1234abcd","background":true,"timeout_seconds":86401}),
        ),
        "empty-handle" => ("send_prompt", json!({"session":"","prompt":"work"})),
        "oversize-handle" => ("wait_session", json!({"session":"s".repeat(33)})),
        "empty-send-prompt" => ("send_prompt", json!({"session":"s-1234abcd","prompt":""})),
        "oversize-send-prompt" => (
            "send_prompt",
            json!({"session":"s-1234abcd","prompt":"é".repeat(8193)}),
        ),
        "short-timeout" => (
            "wait_session",
            json!({"session":"s-1234abcd","timeout_seconds":0}),
        ),
        "long-timeout" => (
            "wait_session",
            json!({"session":"s-1234abcd","timeout_seconds":3601}),
        ),
        "open-no-forward" => ("open_session", json!({"forward_questions":false})),
        "open-forward" => ("open_session", json!({"forward_questions":true})),
        "open-forward-default" => ("open_session", json!({})),
        "invalid-forward-questions" => ("open_session", json!({"forward_questions":"invalid"})),
        "oversize-prompt" => ("open_session", json!({"prompt":"é".repeat(8193)})),
        "oversize-title" => ("open_session", json!({"title":"x".repeat(81)})),
        "oversize-notify-title" => ("notify", json!({"title":"x".repeat(121)})),
        "oversize-body" => ("notify", json!({"title":"Done","body":"é".repeat(1025)})),
        "oversize-summary" => ("request_review", json!({"summary":"é".repeat(2049)})),
        "oversize-paths" => ("request_review", json!({"paths":vec!["a";51]})),
        "relative-cwd" => ("open_session", json!({"cwd":"relative"})),
        _ => return Reply::text("No tool requested."),
    };
    Reply::tool_call(&format!("ompdeck_{tool}"), &args)
}

fn start(tag: &str, tools: Option<&str>) -> Option<Live> {
    let live = Live::start_with_tools(tag, script, |_| {}, |_| None, tools);
    if live.is_none() {
        eprintln!("skipping: OMPDECK_TEST_OMP is not set");
    }
    live
}

fn request(client: &CtlClient) -> ToolRequest {
    wait_for("an OmpDeck tool request", TURN, || {
        client
            .take_tool_requests()
            .into_iter()
            .find_map(|event| match event {
                ToolEvent::Request(request) => Some(request),
                ToolEvent::Cancel(_) => None,
            })
    })
}

fn subscribe(live: &Live, tools: bool) {
    let reply = live.ctl(
        CtlCommand::Subscribe {
            snapshots: true,
            tools,
        },
        TURN,
    );
    assert!(reply.ok, "{reply:?}");
}

fn tool_result(live: &Live, name: &str, entry_start: usize) -> Value {
    wait_for("this turn's persisted tool result", TURN, || {
        live.entries()
            .into_iter()
            .skip(entry_start)
            .rev()
            .find_map(|entry| {
                let message = &entry["message"];
                (message["role"] == "toolResult" && message["toolName"] == name)
                    .then(|| message.clone())
            })
    })
}

fn next_model_result(live: &Live, after: usize, text: &str) -> Value {
    wait_for("the tool result in the next model request", TURN, || {
        live.model.find(after, |r| {
            fm::after_last_user(r)
                .iter()
                .any(|m| fm::role(m) == "tool" && fm::text(m).contains(text))
        })
    })
}

fn stop(live: &Live) {
    let event = wait_for("the composer stop action", TURN, || {
        let doc = live.peek();
        let sf = doc["surfaces"]
            .as_array()?
            .iter()
            .find(|s| s["mode"] == "inline" && s["open"] == true)?["id"]
            .as_str()?
            .to_owned();
        support::nodes(&doc).into_iter().find_map(|(id, _, props)| {
            let role = props["role"].as_str()?;
            if role != "omp.composer.stop" && role != "omp.working.stop" {
                return None;
            }
            Some(Event::Action {
                sf: sf.clone(),
                id,
                act: props["actions"]["click"].as_str()?.to_owned(),
                value: None,
                mods: None,
            })
        })
    });
    live.events(&[event]);
}

#[test]
fn advertises_tools_and_round_trips_notify_with_args_and_root() {
    let Some(live) = start("tools-notify", Some(ALL)) else {
        return;
    };
    assert_eq!(live.client().features(), ["tools", "deliver"]);
    subscribe(&live, true);
    let second = live.another_client();
    wait_for("the second client", TURN, || {
        second.connected().then_some(())
    });
    let rid = second.send(CtlCommand::Subscribe {
        snapshots: false,
        tools: true,
    });
    wait_for("second subscription", TURN, || {
        second
            .take_replies()
            .into_iter()
            .find(|(id, r)| *id == rid && r.ok)
    });
    let before = live.model.count();
    let entry_start = live.entries().len();
    live.send("notify");
    let req = request(live.client());
    assert_eq!(req.name, "notify");
    assert_eq!(
        req.args,
        json!({"title":"Work finished","body":"Ready for review","level":"success"})
    );
    assert_eq!(req.root.as_deref(), Some(live.session_file().as_path()));
    assert!(
        second.take_tool_requests().is_empty(),
        "the first tools subscriber owns the request"
    );
    let model = live
        .model
        .find(before, fm::has_tools)
        .expect("the main model request");
    let offered = fm::tool_names(&model);
    for name in ALL.split(',') {
        assert!(
            offered.contains(&format!("ompdeck_{name}").as_str()),
            "{offered:?}"
        );
    }
    let notify = model["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["function"]["name"] == "ompdeck_notify")
        .unwrap();
    assert_eq!(notify["function"]["parameters"]["type"], "object");
    assert_eq!(
        notify["function"]["parameters"]["properties"]["title"]["type"],
        "string"
    );
    let reply = live.ctl(
        CtlCommand::ToolResult {
            id: req.id,
            ok: true,
            text: RESULT.into(),
            details: Some(json!({"posted":true})),
        },
        TURN,
    );
    assert!(reply.ok, "{reply:?}");
    next_model_result(&live, before, RESULT);
    let result = tool_result(&live, "ompdeck_notify", entry_start);
    assert_eq!(result["isError"], false, "{result}");
    assert_eq!(result["details"], json!({"posted":true}));
    let duplicate = live.ctl(
        CtlCommand::ToolResult {
            id: req.id,
            ok: true,
            text: "late".into(),
            details: None,
        },
        TURN,
    );
    assert_eq!(duplicate.error.as_deref(), Some("no such request"));
}

#[test]
fn review_stop_sends_cancel_and_persists_an_error() {
    let Some(live) = start("tools-cancel", Some("request_review")) else {
        return;
    };
    subscribe(&live, true);
    let entry_start = live.entries().len();
    live.send("review");
    let req = request(live.client());
    assert_eq!(req.name, "request_review");
    stop(&live);
    wait_for("tool_cancel", TURN, || {
        live.client()
            .take_tool_requests()
            .into_iter()
            .any(|event| event == ToolEvent::Cancel(req.id))
            .then_some(())
    });
    let result = tool_result(&live, "ompdeck_request_review", entry_start);
    assert_eq!(result["isError"], true, "{result}");
    assert!(
        result.to_string().contains("Review request cancelled"),
        "{result}"
    );
    let late = live.ctl(
        CtlCommand::ToolResult {
            id: req.id,
            ok: true,
            text: "too late".into(),
            details: None,
        },
        TURN,
    );
    assert_eq!(late.error.as_deref(), Some("no such request"));
}

#[test]
fn no_tools_subscriber_returns_not_connected_after_five_seconds() {
    let Some(live) = start("tools-offline", Some("notify")) else {
        return;
    };
    let before = live.model.count();
    let entry_start = live.entries().len();
    live.send("notify");
    wait_for("the model's tool call", TURN, || {
        live.model.find(before, fm::has_tools)
    });
    let since = Instant::now();
    next_model_result(&live, before, "OmpDeck is not connected to this session");
    assert!(
        since.elapsed() >= Duration::from_millis(4500),
        "returned too early: {:?}",
        since.elapsed()
    );
    assert!(
        since.elapsed() < Duration::from_secs(10),
        "did not honor the five-second connection wait"
    );
    let result = tool_result(&live, "ompdeck_notify", entry_start);
    assert_eq!(result["isError"], true, "{result}");
    assert!(live.client().take_tool_requests().is_empty());
}

#[test]
fn late_subscription_dispatches_waiting_request_and_disconnect_fails_it() {
    let Some(live) = start("tools-drop", Some("notify")) else {
        return;
    };
    let before = live.model.count();
    let entry_start = live.entries().len();
    live.send("notify");
    wait_for("the model's tool call", TURN, || {
        live.model.find(before, fm::has_tools)
    });
    std::thread::sleep(Duration::from_millis(300));
    let client = live.another_client();
    wait_for("the tools client", TURN, || {
        client.connected().then_some(())
    });
    client.send(CtlCommand::Subscribe {
        snapshots: false,
        tools: true,
    });
    assert_eq!(request(&client).name, "notify");
    drop(client);
    next_model_result(&live, before, "OmpDeck disconnected");
    assert_eq!(
        tool_result(&live, "ompdeck_notify", entry_start)["isError"],
        true
    );
}

#[test]
fn unset_launch_policy_exposes_no_ompdeck_tools() {
    let Some(live) = start("tools-unset", None) else {
        return;
    };
    assert!(live.client().features().is_empty());
    let before = live.model.count();
    live.send("ordinary turn");
    let model = wait_for("the main request", TURN, || {
        live.model.find(before, fm::has_tools)
    });
    assert!(
        fm::tool_names(&model)
            .iter()
            .all(|name| !name.starts_with("ompdeck_"))
    );
}

#[test]
fn subagents_never_receive_ompdeck_tools() {
    let Some(live) = start("tools-sub", Some(ALL)) else {
        return;
    };
    let before = live.model.count();
    live.send(fm::TASK_MARK);
    let model = wait_for("the subagent's request", TURN, || {
        live.model.find(before, fm::is_subagent)
    });
    assert!(
        fm::has_tools(&model),
        "a real subagent request, not a title request"
    );
    assert!(
        fm::tool_names(&model)
            .iter()
            .all(|name| !name.starts_with("ompdeck_")),
        "{:?}",
        fm::tool_names(&model)
    );
}

#[test]
fn execute_rejects_oversized_arguments_without_dispatching() {
    let Some(live) = start("tools-limits", Some(ALL)) else {
        return;
    };
    subscribe(&live, true);
    for (prompt, tool, message) in [
        (
            "oversize-prompt",
            "open_session",
            "prompt must be at most 16384 bytes",
        ),
        (
            "oversize-title",
            "open_session",
            "title must be at most 80 characters",
        ),
        (
            "oversize-notify-title",
            "notify",
            "title must be at most 120 characters",
        ),
        ("oversize-body", "notify", "body must be at most 2048 bytes"),
        (
            "oversize-summary",
            "request_review",
            "summary must be at most 4096 bytes",
        ),
        (
            "oversize-paths",
            "request_review",
            "paths must contain at most 50 entries",
        ),
        (
            "relative-cwd",
            "open_session",
            "cwd must be an absolute directory",
        ),
        (
            "empty-handle",
            "send_prompt",
            "session must be a non-empty handle of at most 32 bytes",
        ),
        (
            "oversize-handle",
            "wait_session",
            "session must be a non-empty handle of at most 32 bytes",
        ),
        (
            "empty-send-prompt",
            "send_prompt",
            "prompt must contain 1 to 16384 bytes",
        ),
        (
            "oversize-send-prompt",
            "send_prompt",
            "prompt must be at most 16384 bytes",
        ),
        (
            "short-timeout",
            "wait_session",
            "timeout_seconds must be an integer from 1 to 3600",
        ),
        (
            "long-timeout",
            "wait_session",
            "timeout_seconds must be an integer from 1 to 3600",
        ),
        (
            "short-background-timeout",
            "wait_session",
            "timeout_seconds must be an integer from 1 to 86400",
        ),
        (
            "long-background-timeout",
            "wait_session",
            "timeout_seconds must be an integer from 1 to 86400",
        ),
        (
            "invalid-forward-questions",
            "open_session",
            "forward_questions",
        ),
    ] {
        let before = live.model.count();
        let entry_start = live.entries().len();
        live.send(prompt);
        next_model_result(&live, before, message);
        let result = tool_result(&live, &format!("ompdeck_{tool}"), entry_start);
        assert_eq!(result["isError"], true, "{result}");
        assert!(result.to_string().contains(message), "{result}");
        wait_for("idle", TURN, || live.main_idle().then_some(()));
        assert!(
            live.client().take_tool_requests().is_empty(),
            "invalid arguments escaped execute"
        );
    }
}

#[test]
fn negative_client_result_is_a_tool_error() {
    let Some(live) = start("tools-error", Some("notify")) else {
        return;
    };
    subscribe(&live, true);
    let before = live.model.count();
    let entry_start = live.entries().len();
    live.send("notify");
    let req = request(live.client());
    let reply = live.ctl(
        CtlCommand::ToolResult {
            id: req.id,
            ok: false,
            text: "OmpDeck rejected the request".into(),
            details: None,
        },
        TURN,
    );
    assert!(reply.ok);
    next_model_result(&live, before, "OmpDeck rejected the request");
    assert_eq!(
        tool_result(&live, "ompdeck_notify", entry_start)["isError"],
        true
    );
}

#[test]
fn send_prompt_and_wait_session_are_gated_and_forward_their_arguments() {
    let Some(live) = start("tools-send-wait", Some("send_prompt,wait_session")) else {
        return;
    };
    assert_eq!(
        live.client().tools(),
        ["ompdeck_send_prompt", "ompdeck_wait_session"]
    );
    subscribe(&live, true);
    let before = live.model.count();
    live.send("send-follow-up");
    let send = request(live.client());
    assert_eq!(send.name, "send_prompt");
    assert_eq!(
        send.args,
        json!({"session":"s-1234abcd","prompt":"Continue the implementation","mode":"follow_up"})
    );
    let model = live.model.find(before, fm::has_tools).unwrap();
    let offered = fm::tool_names(&model);
    assert!(offered.contains(&"ompdeck_send_prompt") && offered.contains(&"ompdeck_wait_session"));
    assert!(!offered.contains(&"ompdeck_open_session") && !offered.contains(&"ompdeck_notify"));
    assert!(
        live.ctl(
            CtlCommand::ToolResult {
                id: send.id,
                ok: true,
                text: "Sent to Child".into(),
                details: None
            },
            TURN
        )
        .ok
    );
    next_model_result(&live, before, "Sent to Child");
    wait_for("the sent turn to finish", TURN, || {
        live.main_idle().then_some(())
    });

    let before = live.model.count();
    let entry_start = live.entries().len();
    live.send("wait");
    let wait = request(live.client());
    assert_eq!(wait.name, "wait_session");
    assert_eq!(
        wait.args,
        json!({"session":"s-1234abcd","timeout_seconds":3600})
    );
    // The connection deadline must not become an execution timeout for long-running waits.
    std::thread::sleep(Duration::from_millis(5200));
    assert!(
        !live.main_idle(),
        "wait_session ended before the client supplied a result"
    );
    assert!(
        live.entries()
            .iter()
            .all(|e| e["message"]["toolName"] != "ompdeck_wait_session")
    );
    assert!(
        live.ctl(
            CtlCommand::ToolResult {
                id: wait.id,
                ok: true,
                text: "Child finished with its final reply".into(),
                details: None
            },
            TURN
        )
        .ok
    );
    next_model_result(&live, before, "Child finished with its final reply");
    assert_eq!(
        tool_result(&live, "ompdeck_wait_session", entry_start)["isError"],
        false
    );
}

#[test]
fn wait_session_stop_cancels_the_pending_client_request() {
    let Some(live) = start("tools-wait-cancel", Some("wait_session")) else {
        return;
    };
    assert_eq!(live.client().tools(), ["ompdeck_wait_session"]);
    subscribe(&live, true);
    let entry_start = live.entries().len();
    live.send("wait");
    let wait = request(live.client());
    stop(&live);
    wait_for("wait_session cancellation", TURN, || {
        live.client()
            .take_tool_requests()
            .into_iter()
            .any(|event| event == ToolEvent::Cancel(wait.id))
            .then_some(())
    });
    let result = tool_result(&live, "ompdeck_wait_session", entry_start);
    assert_eq!(result["isError"], true);
    assert!(
        result.to_string().contains("OmpDeck request cancelled"),
        "{result}"
    );
}

#[test]
fn background_wait_accepts_a_day_or_no_timeout_and_forwards_the_flag() {
    let Some(live) = start("tools-background", Some("wait_session")) else {
        return;
    };
    subscribe(&live, true);
    for (prompt, timeout) in [
        ("background-wait", Some(86400)),
        ("background-no-timeout", None),
    ] {
        let before = live.model.count();
        live.send(prompt);
        let req = request(live.client());
        assert_eq!(req.name, "wait_session");
        assert_eq!(req.args["session"], "s-1234abcd");
        assert_eq!(req.args["background"], true);
        assert_eq!(
            req.args.get("timeout_seconds").and_then(Value::as_i64),
            timeout
        );
        assert!(
            live.ctl(
                CtlCommand::ToolResult {
                    id: req.id,
                    ok: true,
                    text: "Watching Child in the background".into(),
                    details: None
                },
                TURN
            )
            .ok
        );
        next_model_result(&live, before, "Watching Child in the background");
        wait_for("background registration turn to finish", TURN, || {
            live.main_idle().then_some(())
        });
    }
}

#[test]
fn deliver_to_an_idle_main_session_starts_a_turn_with_the_message() {
    let Some(live) = start("tools-deliver-idle", Some("wait_session")) else {
        return;
    };
    assert!(live.main_idle());
    subscribe(&live, true);
    let before = live.model.count();
    let wrong_text = "WRONG-ROOT-MUST-NOT-BE-DELIVERED";
    let rejected = live.ctl(
        CtlCommand::Deliver {
            root: live
                .session_file()
                .with_file_name("wrong-conversation.jsonl"),
            title: "Wrong conversation".into(),
            text: wrong_text.into(),
            details: None,
        },
        TURN,
    );
    assert!(!rejected.ok);
    assert_eq!(rejected.code.as_deref(), Some("root_mismatch"));
    let reply = live.ctl(
        CtlCommand::Deliver {
            root: live.session_file(),
            title: "Background wait: Child (s-1234abcd)".into(),
            text: DELIVERED_IDLE.into(),
            details: Some(json!({"session":"s-1234abcd","outcome":"completed"})),
        },
        TURN,
    );
    assert!(reply.ok, "{reply:?}");
    let request = wait_for("idle delivery to start a model request", TURN, || {
        live.model.find(before, |r| {
            fm::has_tools(r)
                && fm::messages(r)
                    .iter()
                    .any(|m| fm::text(m).contains(DELIVERED_IDLE))
        })
    });
    let message = fm::messages(&request)
        .iter()
        .find(|m| fm::text(m).contains(DELIVERED_IDLE))
        .unwrap();
    eprintln!("idle delivered model message: {message}");
    assert_eq!(fm::role(message), "user");
    assert!(live.model.since(before).iter().all(|request| {
        fm::messages(request)
            .iter()
            .all(|message| !fm::text(message).contains(wrong_text))
    }));
    let stored = wait_for("the custom message to be persisted", TURN, || {
        live.entries().into_iter().find(|entry| {
            entry["type"] == "custom_message" && entry["customType"] == "ompdeck-wait"
        })
    });
    assert_eq!(
        stored["content"],
        format!("Background wait: Child (s-1234abcd)\n\n{DELIVERED_IDLE}")
    );
    assert_eq!(stored["display"], true);
    assert_eq!(
        stored["details"],
        json!({"title":"Background wait: Child (s-1234abcd)","session":"s-1234abcd","outcome":"completed"})
    );
    assert!(
        live.entries()
            .iter()
            .all(|entry| !entry.to_string().contains(wrong_text))
    );
}

#[test]
fn deliver_during_a_slow_tool_waits_for_the_boundary_without_aborting_it() {
    let Some(live) = start("tools-deliver-busy", Some("notify,wait_session")) else {
        return;
    };
    subscribe(&live, true);
    let entry_start = live.entries().len();
    live.send("deliver-busy");
    let tool = request(live.client());
    assert_eq!(tool.name, "notify");
    let before = live.model.count();
    let reply = live.ctl(
        CtlCommand::Deliver {
            root: live.session_file(),
            title: "Background wait: Child".into(),
            text: DELIVERED_BUSY.into(),
            details: None,
        },
        TURN,
    );
    assert!(reply.ok, "{reply:?}");
    assert!(
        live.client()
            .take_tool_requests()
            .iter()
            .all(|event| *event != ToolEvent::Cancel(tool.id))
    );
    assert!(
        live.model
            .find(before, |r| fm::has_tools(r)
                && fm::messages(r)
                    .iter()
                    .any(|m| fm::text(m).contains(DELIVERED_BUSY)))
            .is_none(),
        "the aside must wait for the current tool batch"
    );
    let completed = live.ctl(
        CtlCommand::ToolResult {
            id: tool.id,
            ok: true,
            text: "SLOW-TOOL-COMPLETED normally".into(),
            details: None,
        },
        TURN,
    );
    assert!(
        completed.ok,
        "delivery must not cancel the running tool: {completed:?}"
    );
    let request = wait_for(
        "both the tool result and aside at the next step",
        TURN,
        || {
            live.model.find(before, |r| {
                let messages = fm::messages(r);
                fm::has_tools(r)
                    && messages
                        .iter()
                        .any(|m| fm::text(m).contains(DELIVERED_BUSY))
                    && messages.iter().any(|m| {
                        fm::role(m) == "tool"
                            && fm::text(m).contains("SLOW-TOOL-COMPLETED normally")
                    })
            })
        },
    );
    let message = fm::messages(&request)
        .iter()
        .find(|m| fm::text(m).contains(DELIVERED_BUSY))
        .unwrap();
    eprintln!("busy delivered model message: {message}");
    assert_eq!(fm::role(message), "user");
    assert_eq!(
        tool_result(&live, "ompdeck_notify", entry_start)["isError"],
        false
    );
}

#[test]
fn deliver_rejects_non_tools_connections_and_oversized_payloads() {
    let Some(live) = start("tools-del-refuse", Some("wait_session")) else {
        return;
    };
    let denied = live.ctl(
        CtlCommand::Deliver {
            root: live.session_file(),
            title: "Background".into(),
            text: "Result".into(),
            details: None,
        },
        TURN,
    );
    assert_eq!(denied.error.as_deref(), Some("not a tools client"));
    subscribe(&live, true);
    for (title, text, details, reason) in [
        (
            "x".repeat(201),
            "Result".into(),
            None,
            "title must be at most 200 characters",
        ),
        (
            "Background".into(),
            "é".repeat(20 * 1024 + 1),
            None,
            "text must be a string of at most 40960 bytes",
        ),
        (
            "Background".into(),
            "Result".into(),
            Some(json!([])),
            "details must be an object",
        ),
    ] {
        let reply = live.ctl(
            CtlCommand::Deliver {
                root: live.session_file(),
                title,
                text,
                details,
            },
            TURN,
        );
        assert_eq!(reply.error.as_deref(), Some(reason));
    }
    assert!(
        live.entries()
            .iter()
            .all(|entry| entry["customType"] != "ompdeck-wait")
    );
}

#[test]
fn question_tools_are_gated_and_forward_answers_and_open_preferences() {
    let Some(live) = start("tools-questions", Some("open_session,answer_question")) else {
        return;
    };
    assert_eq!(
        live.client().tools(),
        ["ompdeck_open_session", "ompdeck_answer_question"]
    );
    subscribe(&live, true);
    for (prompt, expected) in [
        ("open-no-forward", json!({"forward_questions":false})),
        ("open-forward", json!({"forward_questions":true})),
        ("open-forward-default", json!({})),
    ] {
        let before = live.model.count();
        live.send(prompt);
        let req = request(live.client());
        assert_eq!(req.name, "open_session");
        assert_eq!(req.args, expected);
        let reply = live.ctl(
            CtlCommand::ToolResult {
                id: req.id,
                ok: true,
                text: "Opened Child (s-1234abcd)".into(),
                details: None,
            },
            TURN,
        );
        assert!(reply.ok, "{reply:?}");
        next_model_result(&live, before, "Opened Child (s-1234abcd)");
        wait_for("the open turn to finish", TURN, || {
            live.main_idle().then_some(())
        });
    }
    for mut args in [
        json!({"choice":"2"}),
        json!({"choices":["Blue","1"]}),
        json!({"yes":false}),
        json!({"text":"Use the alternate approach."}),
        json!({"fields":[{"choices":["2"]},{"text":"Other answer"}]}),
        json!({"decision":"once"}),
        json!({"decision":"deny"}),
    ] {
        args["session"] = json!("s-1234abcd");
        args["question"] = json!("q-1234abcd");
        let before = live.model.count();
        let entry_start = live.entries().len();
        live.send(&format!("answer-question {args}"));
        let req = request(live.client());
        assert_eq!(req.name, "answer_question");
        assert_eq!(req.args, args);
        let reply = live.ctl(
            CtlCommand::ToolResult {
                id: req.id,
                ok: true,
                text: "Answered.".into(),
                details: None,
            },
            TURN,
        );
        assert!(reply.ok, "{reply:?}");
        next_model_result(&live, before, "Answered.");
        assert_eq!(
            tool_result(&live, "ompdeck_answer_question", entry_start)["isError"],
            false
        );
        wait_for("the answer turn to finish", TURN, || {
            live.main_idle().then_some(())
        });
    }
}

#[test]
fn answer_question_rejects_invalid_arguments_without_dispatching() {
    let Some(live) = start("tools-ans-bounds", Some("answer_question")) else {
        return;
    };
    subscribe(&live, true);
    for (mut args, reason) in [
        (
            json!({"session":""}),
            "session must be a non-empty handle of at most 32 bytes",
        ),
        (
            json!({"question":""}),
            "question must contain 1 to 32 bytes",
        ),
        (
            json!({"question":"é".repeat(17)}),
            "question must contain 1 to 32 bytes",
        ),
        (
            json!({"choice":"é".repeat(8193)}),
            "choice must be at most 16384 bytes",
        ),
        (
            json!({"text":"é".repeat(8193)}),
            "text must be at most 16384 bytes",
        ),
        (
            json!({"choices":vec!["1";41]}),
            "choices must contain at most 40 choices",
        ),
        (
            json!({"choices":["é".repeat(8193)]}),
            "choices entries must be at most 16384 bytes",
        ),
        (
            json!({"fields":vec![json!({"text":"OK"});41]}),
            "fields must contain at most 40 fields",
        ),
        (
            json!({"fields":[{"text":"é".repeat(8193)}]}),
            "field text must be at most 16384 bytes",
        ),
        (
            json!({"fields":[{"choices":vec!["1";41]}]}),
            "field choices must contain at most 40 choices",
        ),
        (
            json!({"fields":[{"choices":["é".repeat(8193)]}]}),
            "field choices entries must be at most 16384 bytes",
        ),
        (json!({"decision":"session"}), "decision"),
        (json!({"decision":"always"}), "decision"),
    ] {
        if args.get("session").is_none() {
            args["session"] = json!("s-1234abcd");
        }
        if args.get("question").is_none() {
            args["question"] = json!("q-1234abcd");
        }
        let before = live.model.count();
        let entry_start = live.entries().len();
        live.send(&format!("answer-question {args}"));
        next_model_result(&live, before, reason);
        let result = tool_result(&live, "ompdeck_answer_question", entry_start);
        assert_eq!(result["isError"], true, "{result}");
        assert!(result.to_string().contains(reason), "{result}");
        wait_for("the invalid answer turn to finish", TURN, || {
            live.main_idle().then_some(())
        });
        assert!(
            live.client().take_tool_requests().is_empty(),
            "invalid arguments escaped execute: {args}"
        );
    }
}

#[test]
fn snapshot_mirrors_real_todo_phase_updates_and_current_task() {
    let Some(live) = start("tools-progress", None) else {
        return;
    };
    assert!(live.client().latest().unwrap().progress.is_none());
    let entry_start = live.entries().len();
    live.send("todo-progress");
    let progress = wait_for("the live started todo task", TURN, || {
        let progress = live.client().latest()?.progress.clone()?;
        (progress.counts() == (1, 3)
            && progress.current().is_some_and(|t| t.content == "Run tests"))
        .then_some(progress)
    });
    assert_eq!(progress.phases.len(), 2);
    assert_eq!(progress.phases[0].name, "Implement");
    assert_eq!(progress.phases[1].name, "Verify");
    assert_eq!(progress.phases[0].tasks[0].status, TodoStatus::Completed);
    assert_eq!(progress.current().unwrap().status, TodoStatus::InProgress);
    assert!(progress.goal.is_none());
    assert!(!progress.truncated);
    let result = tool_result(&live, "todo", entry_start);
    assert_eq!(result["isError"], false, "{result}");
    assert_eq!(result["details"]["op"], "start");
}

#[test]
fn snapshot_caps_real_todo_phases_tasks_and_unicode_text() {
    let Some(live) = start("todo-caps", None) else {
        return;
    };
    let list: Vec<_> = (0..17)
        .map(|phase| json!({
            "phase": format!("Phase {phase} {}", "🦀".repeat(81)),
            "items": (0..6).map(|task| format!("{phase}-{task} {}", "🦀".repeat(201))).collect::<Vec<_>>()
        }))
        .collect();
    live.send(&format!(
        "agent-tool {}",
        json!({"name":"todo","args":{"op":"init","list":list}})
    ));
    let progress = wait_for("capped todo progress", TURN, || {
        let progress = live.client().latest()?.progress.clone()?;
        progress.truncated.then_some(progress)
    });
    assert_eq!(progress.phases.len(), 16);
    assert_eq!(progress.counts(), (0, 80));
    for phase in &progress.phases {
        assert_eq!(phase.name.chars().count(), 80);
        assert!(phase.name.ends_with('…'));
        for task in &phase.tasks {
            assert_eq!(task.content.chars().count(), 200);
            assert!(task.content.ends_with('…'));
        }
    }
}

#[test]
fn worktree_and_report_tools_are_gated_and_forward_valid_arguments() {
    let Some(live) = start(
        "tools-wt-report",
        Some("open_session,finish_worktree,report"),
    ) else {
        return;
    };
    assert_eq!(
        live.client().tools(),
        [
            "ompdeck_open_session",
            "ompdeck_finish_worktree",
            "ompdeck_report"
        ]
    );
    subscribe(&live, true);
    for (name, args, text) in [
        (
            "open_session",
            json!({"prompt":"Implement independently","worktree":{}}),
            "Opened Child. Worktree: /tmp/child on branch wt/test (merges into main)",
        ),
        (
            "open_session",
            json!({"worktree":{"branch":"wt/child","base":"main"}}),
            "Opened Child. Worktree: /tmp/child on branch wt/child (merges into main)",
        ),
        (
            "open_session",
            json!({"worktree":{"branch":"x".repeat(200),"base":"y".repeat(200)}}),
            "Opened boundary child.",
        ),
        (
            "finish_worktree",
            json!({"session":"s-1234abcd"}),
            "Finish started: Child was asked to commit and merge wt/child into main. OmpDeck removes the worktree once it is clean and merged; the outcome arrives as a message.",
        ),
        (
            "report",
            json!({"text":"Implementation is ready"}),
            "Reported to Parent.",
        ),
        (
            "report",
            json!({"text":"Waiting for credentials","kind":"blocked"}),
            "Reported to Parent.",
        ),
        (
            "report",
            json!({"text":"Merged and verified","kind":"done"}),
            "Reported to Parent.",
        ),
        (
            "report",
            json!({"text":"é".repeat(2048),"kind":"progress"}),
            "Reported to Parent.",
        ),
        ("report", json!({"text":"x"}), "Reported to Parent."),
    ] {
        let before = live.model.count();
        let entry_start = live.entries().len();
        live.send(&format!(
            "agent-tool {}",
            json!({"name":format!("ompdeck_{name}"),"args":args})
        ));
        let req = request(live.client());
        assert_eq!(req.name, name);
        assert_eq!(req.args, args);
        assert_eq!(req.root.as_deref(), Some(live.session_file().as_path()));
        let model = live.model.find(before, fm::has_tools).unwrap();
        let offered: Vec<_> = fm::tool_names(&model)
            .into_iter()
            .filter(|n| n.starts_with("ompdeck_"))
            .collect();
        assert_eq!(offered.len(), 3);
        for tool in [
            "ompdeck_open_session",
            "ompdeck_finish_worktree",
            "ompdeck_report",
        ] {
            assert!(offered.contains(&tool), "{offered:?}");
        }
        assert!(
            live.ctl(
                CtlCommand::ToolResult {
                    id: req.id,
                    ok: true,
                    text: text.into(),
                    details: None,
                },
                TURN
            )
            .ok
        );
        next_model_result(&live, before, text);
        assert_eq!(
            tool_result(&live, &format!("ompdeck_{name}"), entry_start)["isError"],
            false
        );
        wait_for("the tool turn to finish", TURN, || {
            live.main_idle().then_some(())
        });
    }
}

#[test]
fn worktree_finish_and_report_reject_invalid_arguments_without_dispatching() {
    let Some(live) = start(
        "tools-wt-limits",
        Some("open_session,finish_worktree,report"),
    ) else {
        return;
    };
    subscribe(&live, true);
    for (name, args, reason) in [
        ("open_session", json!({"worktree":"invalid"}), "worktree"),
        (
            "open_session",
            json!({"worktree":{"branch":"x".repeat(201)}}),
            "worktree",
        ),
        (
            "open_session",
            json!({"worktree":{"base":"x".repeat(201)}}),
            "worktree",
        ),
        (
            "finish_worktree",
            json!({"session":""}),
            "session must be a non-empty handle of at most 32 bytes",
        ),
        (
            "finish_worktree",
            json!({"session":"é".repeat(17)}),
            "session must be a non-empty handle of at most 32 bytes",
        ),
        (
            "report",
            json!({"text":""}),
            "text must contain 1 to 4096 bytes",
        ),
        (
            "report",
            json!({"text":"é".repeat(2049)}),
            "text must contain 1 to 4096 bytes",
        ),
        ("report", json!({"text":"Ready","kind":"invalid"}), "kind"),
    ] {
        let before = live.model.count();
        let tool = format!("ompdeck_{name}");
        let entry_start = live.entries().len();
        live.send(&format!("agent-tool {}", json!({"name":tool,"args":args})));
        let result = wait_for(&format!("rejection of {name} {args}"), TURN, || {
            assert!(
                live.client().take_tool_requests().is_empty(),
                "invalid arguments dispatched: {name} {args}"
            );
            live.entries()
                .into_iter()
                .skip(entry_start)
                .find_map(|entry| {
                    let message = &entry["message"];
                    (message["role"] == "toolResult" && message["toolName"] == tool)
                        .then(|| message.clone())
                })
        });
        assert_eq!(result["isError"], true, "{result}");
        assert!(result.to_string().contains(reason), "{result}");
        next_model_result(&live, before, reason);
        wait_for("the rejected tool turn to finish", TURN, || {
            live.main_idle().then_some(())
        });
        assert!(
            live.client().take_tool_requests().is_empty(),
            "invalid arguments escaped: {args}"
        );
    }
}

#[test]
fn omp_normalizes_worktree_fields_before_extension_validation() {
    let Some(live) = start("tools-wt-coerce", Some("open_session")) else {
        return;
    };
    subscribe(&live, true);
    let before = live.model.count();
    live.send(&format!(
        "agent-tool {}",
        json!({
            "name": "ompdeck_open_session",
            "args": {"worktree": {"branch": 17, "base": false}}
        })
    ));
    let req = request(live.client());
    assert_eq!(req.name, "open_session");
    // omp repairs model arguments against the tool schema before execute receives them.
    assert_eq!(req.args, json!({"worktree":{"branch":"17","base":"false"}}));
    assert!(
        live.ctl(
            CtlCommand::ToolResult {
                id: req.id,
                ok: true,
                text: "Opened normalized child.".into(),
                details: None,
            },
            TURN
        )
        .ok
    );
    next_model_result(&live, before, "Opened normalized child.");
}

#[test]
fn snapshot_keeps_completed_goals_until_omp_clears_their_state() {
    let Some(live) = start("goal-complete", None) else {
        return;
    };
    assert!(live.client().latest().unwrap().progress.is_none());
    live.send("/goal goal-progress-completion");
    wait_for("the active goal", TURN, || {
        let progress = live.client().latest()?.progress.clone()?;
        (progress.goal.as_ref()?.status == GoalStatus::Active).then_some(())
    });
    let completed = wait_for("the disabled completed goal", TURN, || {
        let progress = live.client().latest()?.progress.clone()?;
        (progress.goal.as_ref()?.status == GoalStatus::Complete).then_some(progress)
    });
    assert_eq!(
        completed.goal.as_ref().unwrap().objective,
        "goal-progress-completion"
    );
    assert_eq!(completed.counts(), (0, 0));
    assert!(!completed.is_empty());
    wait_for(
        "omp to clear the completed goal after the turn",
        TURN,
        || {
            let snapshot = live.client().latest()?;
            (snapshot.main_idle == Some(true) && snapshot.progress.is_none()).then_some(())
        },
    );
}

fn git(repo: &std::path::Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .args(["--no-optional-locks", "-c", "core.fsmonitor=false", "-C"])
        .arg(repo)
        .args(args)
        .env_remove("GIT_INDEX_FILE")
        .env("LC_ALL", "C")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .unwrap()
        .trim_end_matches(['\r', '\n'])
        .to_owned()
}

#[test]
fn checkpoint_precedes_real_writes_once_per_run_and_preserves_the_real_index() {
    let Some(live) = Live::start_with_policy(
        "cp-write",
        script,
        |live| {
            git(&live.proj, &["init", "--initial-branch=main"]);
            git(&live.proj, &["config", "user.name", "Checkpoint test"]);
            git(
                &live.proj,
                &["config", "user.email", "checkpoint@example.invalid"],
            );
            git(&live.proj, &["config", "core.autocrlf", "false"]);
            std::fs::write(live.proj.join("tracked.txt"), "committed\n").unwrap();
            git(&live.proj, &["add", "-A"]);
            git(&live.proj, &["commit", "-m", "fixture"]);
            std::fs::write(live.proj.join("tracked.txt"), "staged\n").unwrap();
            git(&live.proj, &["add", "tracked.txt"]);
            git(
                &live.proj,
                &["update-index", "--assume-unchanged", "tracked.txt"],
            );
            std::fs::write(live.proj.join("tracked.txt"), "before the agent\n").unwrap();
        },
        |_| None,
        None,
        true,
    ) else {
        return;
    };
    let index = std::path::PathBuf::from(git(
        &live.proj,
        &["rev-parse", "--path-format=absolute", "--git-path", "index"],
    ));
    let before = std::fs::read(&index).unwrap();
    for expected_seq in 1..=2 {
        let entry_start = live.entries().len();
        live.send("checkpoint-edit");
        let write = tool_result(&live, "write", entry_start);
        assert_eq!(write["isError"], false, "{write}");
        wait_for("both checkpointed writes to finish", TURN, || {
            let entries = live.entries();
            let finished = entries.iter().skip(entry_start).any(|entry| {
                entry["message"]["role"] == "assistant"
                    && fm::text(&entry["message"]).contains("Checkpointed writes finished.")
            });
            let leaf = entries.last()?.get("id")?.as_str()?;
            (finished
                && live.client().latest()?.main_idle == Some(true)
                && live.client().latest()?.leaf.as_deref() == Some(leaf))
            .then_some(())
        });
        let session = git(
            &live.proj,
            &[
                "for-each-ref",
                "--format=%(refname)",
                "refs/ompdeck/checkpoints/",
            ],
        );
        let session = session
            .lines()
            .next()
            .unwrap_or_default()
            .split('/')
            .nth(3)
            .unwrap_or_default();
        let items = checkpoints::list(&live.proj, session).unwrap();
        assert_eq!(
            items.len(),
            expected_seq,
            "{}",
            std::fs::read_to_string(live.root.join("checkpoint.log")).unwrap_or_default()
        );
        let newest = &items[0];
        assert_eq!(newest.seq as usize, expected_seq);
        assert_eq!(newest.kind, CheckpointKind::Turn);
        let user = live
            .entries()
            .into_iter()
            .skip(entry_start)
            .find(|e| e["type"] == "message" && e["message"]["role"] == "user")
            .unwrap();
        let prompt: String = fm::text(&user["message"])
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .chars()
            .take(120)
            .collect();
        assert_eq!(newest.prompt, prompt);
        assert_eq!(newest.entry.as_deref(), user["id"].as_str());
        assert_eq!(
            git(
                &live.proj,
                &["show", &format!("{}:tracked.txt", newest.commit)]
            ),
            if expected_seq == 1 {
                "before the agent"
            } else {
                "agent changed the file"
            }
        );
        assert_eq!(std::fs::read(&index).unwrap(), before);
    }
}

#[test]
fn fan_out_is_gated_and_forwards_tasks_and_group_options() {
    let Some(live) = start("fan-out", Some("fan_out")) else {
        return;
    };
    assert_eq!(live.client().tools(), ["ompdeck_fan_out"]);
    subscribe(&live, true);
    for args in [
        json!({"tasks":[{"prompt":"One independent task"}]}),
        json!({"tasks":[
            {"prompt":"Implement backend","title":"Backend","model":"fake/fake-1","branch":"wt/backend"},
            {"prompt":"Implement frontend","title":"Frontend","branch":"wt/frontend"}
        ],"worktree":true,"base":"main","cwd":live.proj,"forward_questions":false,"timeout_seconds":86400}),
        json!({"tasks":vec![json!({"prompt":"Independent task"});8],"worktree":false}),
    ] {
        let before = live.model.count();
        let entry_start = live.entries().len();
        live.send(&format!(
            "agent-tool {}",
            json!({"name":"ompdeck_fan_out","args":args})
        ));
        let request = request(live.client());
        assert_eq!(request.name, "fan_out");
        assert_eq!(request.args, args);
        let model = live.model.find(before, fm::has_tools).unwrap();
        let tools: Vec<_> = fm::tool_names(&model)
            .into_iter()
            .filter(|name| name.starts_with("ompdeck_"))
            .collect();
        assert_eq!(tools, ["ompdeck_fan_out"]);
        let text = "1. s-1234abcd — Backend — wt/backend\nTheir combined result arrives as one message when all have finished.";
        assert!(
            live.ctl(
                CtlCommand::ToolResult {
                    id: request.id,
                    ok: true,
                    text: text.into(),
                    details: None
                },
                TURN
            )
            .ok
        );
        next_model_result(&live, before, text);
        assert_eq!(
            tool_result(&live, "ompdeck_fan_out", entry_start)["isError"],
            false
        );
        wait_for("the fan-out call to finish", TURN, || {
            live.main_idle().then_some(())
        });
    }
}

#[test]
fn fan_out_rejects_task_and_group_limits_without_dispatching() {
    let Some(live) = start("fan-limits", Some("fan_out")) else {
        return;
    };
    subscribe(&live, true);
    for (args, reason) in [
        (json!({"tasks":[]}), "tasks"),
        (json!({"tasks":vec![json!({"prompt":"task"});9]}), "tasks"),
        (json!({"tasks":[{"prompt":""}]}), "each task prompt"),
        (
            json!({"tasks":[{"prompt":"é".repeat(8193)}]}),
            "each task prompt",
        ),
        (
            json!({"tasks":[{"prompt":"task","title":"x".repeat(81)}]}),
            "task title",
        ),
        (
            json!({"tasks":[{"prompt":"task","branch":"x".repeat(201)}]}),
            "branch",
        ),
        (
            json!({"tasks":[{"prompt":"task"}],"base":"x".repeat(201)}),
            "base",
        ),
        (json!({"tasks":[{"prompt":"task"}],"cwd":"relative"}), "cwd"),
        (
            json!({"tasks":[{"prompt":"task"}],"timeout_seconds":0}),
            "timeout_seconds",
        ),
        (
            json!({"tasks":[{"prompt":"task"}],"timeout_seconds":86401}),
            "timeout_seconds",
        ),
    ] {
        let before = live.model.count();
        let entry_start = live.entries().len();
        live.send(&format!(
            "agent-tool {}",
            json!({"name":"ompdeck_fan_out","args":args})
        ));
        let result = tool_result(&live, "ompdeck_fan_out", entry_start);
        assert_eq!(result["isError"], true, "{result}");
        assert!(result.to_string().contains(reason), "{result}");
        next_model_result(&live, before, reason);
        wait_for("the refused fan-out call", TURN, || {
            live.main_idle().then_some(())
        });
        assert!(
            live.client().take_tool_requests().is_empty(),
            "invalid fan-out dispatched: {args}"
        );
    }
}
