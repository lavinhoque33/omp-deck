//! Real omp extension tools through the gateway, driven by the scripted model.
//! Uses only a private HOME/XDG tree and test host/tmux namespace.

mod support;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use deck_core::config::SessionView;
use deck_service::controller::{Command, Delivery, SessionTarget};
use deck_service::gateway::client::{ClientEvent, GatewayClient, GatewayOptions, Link};
use deck_service::gateway::protocol::{
    Answer, Done, LaunchSpec, Mutation, Outcome, Query, ReviewReply, Role, SessionRole, Topic,
    TopicValue,
};
use serde_json::{Value, json};
use support::device::{GATEWAY, OMP_CONFIG, STEP, Sandbox, run, wait_for};
use support::fake_model::{self as fm, FakeModel, Reply};

const TURN: Duration = Duration::from_secs(90);

fn which(program: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(program))
        .find(|path| path.is_file())
}

fn script(req: &Value) -> Reply {
    if !fm::has_tools(req) {
        return Reply::text("Fake title");
    }
    let prompt = fm::last_user_text(req);
    if fm::after_last_user(req)
        .iter()
        .any(|m| fm::role(m) == "tool")
    {
        return Reply::text("Tool completed.");
    }
    let (name, args) = if prompt.contains("SHOW-MARK") {
        ("ompdeck_show_changes", json!({}))
    } else if prompt.contains("NOTIFY-MARK") {
        (
            "ompdeck_notify",
            json!({"title": "Agent live test", "body": "Notification body", "level": "info"}),
        )
    } else if prompt.contains("SESSIONS-MARK") {
        ("ompdeck_sessions", json!({}))
    } else if prompt.contains("REVIEW-MARK") {
        (
            "ompdeck_request_review",
            json!({"summary": "Please review the live test", "paths": ["notes.txt"]}),
        )
    } else {
        return Reply::text("Unscripted prompt");
    };
    assert!(
        fm::tool_names(req).contains(&name),
        "extension must advertise {name}"
    );
    Reply::tool_call(name, &args)
}

fn launch(client: &GatewayClient, sb: &Sandbox, name: &str, prompt: &str) -> SessionTarget {
    let done = run(
        client,
        Mutation::Launch(LaunchSpec {
            mux_name: Some(format!("omp-tools-{name}")),
            cwd: sb.proj(),
            session_path: None,
            label: format!("Agent tools {name}"),
            argv: vec!["omp".into(), prompt.into()],
            role: SessionRole::Session,
            copy_of: None,
            view: SessionView::Chat,
        }),
    )
    .expect("launch real omp");
    let Done::Session(target, view) = done else {
        panic!("launch result: {done:?}")
    };
    assert_eq!(view, SessionView::Chat);
    target
}

/// The model's next completion proves omp received the extension's real tool result.
fn result(model: &FakeModel, mark: &str) -> String {
    let request = wait_for("tool result returned to the model", TURN, || {
        model.find(0, |req| {
            fm::last_user_text(req).contains(mark)
                && fm::after_last_user(req)
                    .iter()
                    .any(|m| fm::role(m) == "tool")
        })
    });
    fm::after_last_user(&request)
        .iter()
        .filter(|m| fm::role(m) == "tool")
        .map(fm::text)
        .collect::<Vec<_>>()
        .join("\n")
}

fn exercise_notify_list_review_and_require_a_window() {
    let Some(_omp) = which(if cfg!(windows) { "omp.exe" } else { "omp" }) else {
        eprintln!("skipped: omp is not installed");
        return;
    };
    let config = format!("{OMP_CONFIG}modelRoles:\n  default: {}\n", fm::MODEL);
    let sb = Sandbox::new("tools", &config);
    sb.report_failures();
    // Sandbox selects the sibling host binary, just as the packaged gateway does.
    let host = PathBuf::from(GATEWAY).with_file_name(if cfg!(windows) {
        "omp-deck-host.exe"
    } else {
        "omp-deck-host"
    });
    assert!(
        host.is_file(),
        "build deck-host before running this live test"
    );
    let model = FakeModel::start(script);
    std::fs::write(sb.agent().join("models.yml"), model.models_yml()).expect("models.yml");

    // A CLI can launch sessions but is not a desktop tool handler.
    let mut opts = GatewayOptions::new(sb.profile.clone(), Role::Cli, Arc::new(|| {}));
    opts.autostart = true;
    opts.gateway_exe = Some(PathBuf::from(GATEWAY));
    let cli = GatewayClient::start(opts);
    wait_for("CLI gateway link", STEP, || {
        matches!(cli.link(), Link::Up(_)).then_some(())
    });
    let show = launch(&cli, &sb, "show", "SHOW-MARK open changes");
    let text = result(&model, "SHOW-MARK");
    assert!(
        text.contains("OmpDeck's window is not open"),
        "no-GUI error: {text}"
    );

    let gui = sb.client();
    wait_for("GUI gateway link", STEP, || {
        matches!(gui.link(), Link::Up(_)).then_some(())
    });
    // Deliberately never subscribe to Topic::Live: the tool link must survive without it.
    let notify = launch(&cli, &sb, "notify", "NOTIFY-MARK tell the user");
    assert_eq!(result(&model, "NOTIFY-MARK"), "Posted");
    wait_for("agent notification inbox entry", STEP, || {
        let answer = gui.query(Query::Topic(Topic::Notifications)).wait(STEP)?;
        let Ok(Answer::Topic(value)) = answer else {
            return None;
        };
        let TopicValue::Notifications(inbox) = *value else {
            return None;
        };
        inbox
            .entries
            .iter()
            .any(|entry| {
                entry.target == notify
                    && entry.title == "Agent live test"
                    && entry.body == "Notification body"
            })
            .then_some(())
    });
    let sessions = launch(&cli, &sb, "sessions", "SESSIONS-MARK list sessions");
    let text = result(&model, "SESSIONS-MARK");
    assert!(
        text.contains("Agent tools sessions"),
        "own session must be listed: {text}"
    );
    assert!(
        text.contains("~/proj"),
        "session cwd must be abbreviated: {text}"
    );

    let passive = sb.client();
    wait_for("second GUI link", STEP, || {
        matches!(passive.link(), Link::Up(_)).then_some(())
    });
    wait_for("empty replay boundary", STEP, || {
        passive.events().into_iter().find_map(|event| match event {
            ClientEvent::AgentReviewsReplayed { pending } => {
                assert!(pending.is_empty());
                Some(())
            }
            _ => None,
        })
    });
    gui.set_client_state(deck_service::gateway::protocol::ClientState {
        focused: true,
        ..Default::default()
    });
    // Same-connection round trip makes focus effective before the CLI launches the review.
    gui.query(Query::Topic(Topic::Notifications))
        .wait(STEP)
        .expect("focus barrier")
        .expect("query");
    let review = launch(&cli, &sb, "review", "REVIEW-MARK ask for review");
    let request = wait_for("GUI review request", TURN, || {
        gui.events().into_iter().find_map(|event| match event {
            ClientEvent::AgentReviewRequest {
                request,
                activate,
                target,
                summary,
                paths,
            } if target == review => {
                assert!(activate, "first review delivery designates the only GUI");
                assert_eq!(summary.as_deref(), Some("Please review the live test"));
                assert_eq!(paths, vec!["notes.txt"]);
                Some(request)
            }
            _ => None,
        })
    });
    wait_for("passive review delivery", STEP, || {
        passive.events().into_iter().find_map(|event| match event {
            ClientEvent::AgentReviewRequest {
                request: id,
                activate,
                ..
            } if id == request => {
                assert!(!activate, "only the designated GUI activates");
                Some(())
            }
            _ => None,
        })
    });
    let replay = sb.client();
    let mut replayed = false;
    wait_for("ordered pending review replay", STEP, || {
        for event in replay.events() {
            match event {
                ClientEvent::AgentReviewRequest {
                    request: id,
                    activate,
                    ..
                } if id == request => {
                    assert!(!activate, "reconnect replay never activates");
                    replayed = true;
                }
                ClientEvent::AgentReviewsReplayed { pending } => {
                    assert_eq!(pending, vec![request]);
                    assert!(replayed, "boundary must follow the pending request");
                    return Some(());
                }
                _ => {}
            }
        }
        None
    });
    // A stale/wrong-session GUI response must not resolve this request.
    let wrong = gui.agent_review_reply(request, sessions.clone(), ReviewReply::NoComments);
    assert!(matches!(wrong.accepted.wait(STEP), Some(Ok(_))));
    assert!(matches!(
        wrong.outcome.wait(STEP),
        Some(Ok(Outcome::Done(_)))
    ));
    assert!(
        model
            .find(0, |req| {
                fm::last_user_text(req).contains("REVIEW-MARK")
                    && fm::after_last_user(req)
                        .iter()
                        .any(|m| fm::role(m) == "tool")
            })
            .is_none()
    );
    let reply = gui.agent_review_reply(request, review.clone(), ReviewReply::NoComments);
    assert!(matches!(reply.accepted.wait(STEP), Some(Ok(_))));
    assert!(matches!(
        reply.outcome.wait(STEP),
        Some(Ok(Outcome::Done(_)))
    ));
    assert_eq!(result(&model, "REVIEW-MARK"), "The user had no comments");

    for target in [show, notify, sessions, review] {
        run(&cli, Mutation::Kill(target)).expect("stop test session");
    }
}

const FIRST_REPLY: &str = "The complete first child reply, including its final sentence.";
const SECOND_REPLY: &str = "The complete second child reply, including its final sentence.";
const FOLLOW_REPLY: &str = "The queued follow-up reply, including its final sentence.";

fn tool_texts(req: &Value) -> Vec<String> {
    fm::after_last_user(req)
        .iter()
        .filter(|message| fm::role(message) == "tool")
        .map(fm::text)
        .collect()
}

fn session_handle(req: &Value) -> String {
    fm::messages(req)
        .iter()
        .filter(|message| fm::role(message) == "tool")
        .flat_map(|message| {
            fm::text(message)
                .split_whitespace()
                .map(|word| word.trim_end_matches(':').to_owned())
                .collect::<Vec<_>>()
        })
        .find(|word| {
            word.len() == 10
                && word.starts_with("s-")
                && word[2..].bytes().all(|byte| byte.is_ascii_hexdigit())
        })
        .expect("open_session must return a usable s-xxxxxxxx handle")
}

fn send_wait_script(req: &Value) -> Reply {
    if !fm::has_tools(req) {
        return Reply::text("Fake title");
    }
    let prompt = fm::last_user_text(req);
    if prompt.contains("CHILD-FIRST") {
        return Reply::slow_text(FIRST_REPLY, Duration::from_secs(3));
    }
    if prompt.contains("CHILD-SECOND") {
        return Reply::slow_text(SECOND_REPLY, Duration::from_secs(3));
    }
    if prompt.contains("CHILD-SLOW") {
        return Reply::slow_text(
            "The slow turn finished before the queued turn.",
            Duration::from_secs(20),
        );
    }
    if prompt.contains("CHILD-FOLLOW") {
        return Reply::slow_text(FOLLOW_REPLY, Duration::from_secs(3));
    }
    if prompt.contains("CHILD-HEADLESS") {
        return Reply::text("The headlessly reattached child replied.");
    }
    let results = tool_texts(req);
    let (name, args) = if prompt.contains("DRIVE-MARK") {
        match results.len() {
            0 => (
                "ompdeck_open_session",
                json!({"prompt": "CHILD-FIRST", "title": "Driven child", "placement": "tab"}),
            ),
            1 | 4 | 7 => (
                "ompdeck_wait_session",
                json!({"session": session_handle(req), "timeout_seconds": 60}),
            ),
            2 => ("ompdeck_sessions", json!({})),
            3 => (
                "ompdeck_send_prompt",
                json!({"session": session_handle(req), "prompt": "CHILD-SECOND", "mode": "send"}),
            ),
            5 => (
                "ompdeck_send_prompt",
                json!({"session": session_handle(req), "prompt": "CHILD-SLOW"}),
            ),
            6 => {
                // Let the child's submitted prompt reach the model before queuing this turn.
                return Reply::slow_tool_call(
                    "ompdeck_send_prompt",
                    &json!({"session": session_handle(req), "prompt": "CHILD-FOLLOW", "mode": "follow_up"}),
                    Duration::from_secs(2),
                );
            }
            _ => return Reply::text("Driver completed."),
        }
    } else if prompt.contains("HEADLESS-MARK") {
        match results.len() {
            0 => (
                "ompdeck_send_prompt",
                json!({"session": session_handle(req), "prompt": "CHILD-HEADLESS"}),
            ),
            1 | 2 => (
                "ompdeck_wait_session",
                json!({"session": session_handle(req), "timeout_seconds": 60}),
            ),
            _ => return Reply::text("Headless driver completed."),
        }
    } else if prompt.contains("DRAFT-MARK") || prompt.contains("STALE-MARK") {
        if !results.is_empty() {
            return Reply::text("Refusal received.");
        }
        (
            "ompdeck_send_prompt",
            json!({"session": session_handle(req), "prompt": "MUST-NOT-BE-SENT"}),
        )
    } else {
        return Reply::text("Unscripted prompt");
    };
    assert!(
        fm::tool_names(req).contains(&name),
        "extension must advertise {name}"
    );
    Reply::tool_call(name, &args)
}

fn idle(client: &GatewayClient, target: &SessionTarget) {
    wait_for("live idle controller", TURN, || {
        let Ok(Answer::Topic(value)) = client.query(Query::Topic(Topic::Controllers)).wait(STEP)?
        else {
            return None;
        };
        let TopicValue::Controllers(controllers) = *value else {
            return None;
        };
        controllers
            .iter()
            .any(|info| {
                &info.target == target
                    && info.live_control
                    && !info.busy
                    && !info.activity.is_busy()
                    && info.exit_code.is_none()
            })
            .then_some(())
    });
}

fn prompt(client: &GatewayClient, target: &SessionTarget, text: &str) {
    idle(client, target);
    let done = run(
        client,
        Mutation::Submit {
            target: target.clone(),
            command: Command::Prompt { text: text.into() },
        },
    )
    .expect("submit opener prompt");
    assert!(
        matches!(&done, Done::Submitted(record) if record.delivery == Delivery::Delivered),
        "{done:?}"
    );
}

fn exercise_open_wait_send_follow_up_draft_and_stale_handle() {
    let Some(_omp) = which(if cfg!(windows) { "omp.exe" } else { "omp" }) else {
        eprintln!("skipped: omp is not installed");
        return;
    };
    let config = format!("{OMP_CONFIG}modelRoles:\n  default: {}\n", fm::MODEL);
    let sb = Sandbox::new("ts", &config);
    sb.report_failures();
    let model = FakeModel::start(send_wait_script);
    std::fs::write(sb.agent().join("models.yml"), model.models_yml()).expect("models.yml");
    let gui = sb.client();
    wait_for("GUI gateway link", STEP, || {
        matches!(gui.link(), Link::Up(_)).then_some(())
    });
    let opener = launch(&gui, &sb, "driver", "DRIVE-MARK drive a parallel session");
    let (request, mut child) = wait_for("GUI open-session request", TURN, || {
        gui.events().into_iter().find_map(|event| match event {
            ClientEvent::AgentOpenSession {
                request,
                opener: target,
                cwd,
                prompt,
                model,
                title,
                ..
            } if target == opener => {
                assert_eq!(cwd, sb.proj());
                assert_eq!(prompt.as_deref(), Some("CHILD-FIRST"));
                assert!(model.is_none());
                assert_eq!(title.as_deref(), Some("Driven child"));
                // Match the real GUI: Launch carries the initial prompt in argv, then
                // AgentToolDone associates the resulting child with the opener's root.
                let done = run(
                    &gui,
                    Mutation::Launch(LaunchSpec {
                        mux_name: Some("omp-tools-driven-child".into()),
                        cwd,
                        session_path: None,
                        label: title.expect("child title"),
                        argv: vec!["omp".into(), prompt.expect("initial child prompt")],
                        role: SessionRole::Session,
                        copy_of: None,
                        view: SessionView::Chat,
                    }),
                )
                .expect("GUI launches child");
                let Done::Session(child, SessionView::Chat) = done else {
                    panic!("child launch result: {done:?}");
                };
                Some((request, child))
            }
            _ => None,
        })
    });
    run(
        &gui,
        Mutation::AgentToolDone {
            request,
            target: opener.clone(),
            ok: true,
            text: "Opened Driven child.".into(),
            opened: Some(child.clone()),
        },
    )
    .expect("GUI completes open-session tool");
    let completed = wait_for("all driver tool results", TURN, || {
        model.find(0, |req| {
            fm::last_user_text(req).contains("DRIVE-MARK") && tool_texts(req).len() == 8
        })
    });
    let results = tool_texts(&completed);
    let handle = session_handle(&completed);
    assert!(
        results[0].contains(&format!("Handle: {handle}")),
        "{results:?}"
    );
    for (index, reply) in [(1, FIRST_REPLY), (4, SECOND_REPLY), (7, FOLLOW_REPLY)] {
        assert!(
            results[index].contains("finished (completed)"),
            "{}",
            results[index]
        );
        assert!(results[index].ends_with(reply), "{}", results[index]);
    }
    assert!(
        results[2].contains(&format!("handle: {handle}")),
        "{}",
        results[2]
    );
    assert!(
        results[2]
            .lines()
            .any(|row| { row.contains(&format!("handle: {handle}")) && row.contains(FIRST_REPLY) }),
        "the child's session row must carry its handle and reply: {}",
        results[2]
    );
    for index in [3, 5, 6] {
        assert!(results[index].contains("Sent to"), "{}", results[index]);
    }
    // Closing the controller leaves the host process alive. The same handle must attach it
    // without a GUI tab, survive the new generation, and support idempotent waits.
    run(
        &gui,
        Mutation::Close {
            target: child.clone(),
            kill_direct: false,
        },
    )
    .expect("close child controller");
    prompt(
        &gui,
        &opener,
        "HEADLESS-MARK send after closing the child tab",
    );
    let reattached = wait_for("headless child reply and repeat wait", TURN, || {
        model.find(0, |req| {
            fm::last_user_text(req).contains("HEADLESS-MARK") && tool_texts(req).len() == 3
        })
    });
    let headless_results = tool_texts(&reattached);
    assert!(
        headless_results[0].contains("Sent to"),
        "{headless_results:?}"
    );
    for reply in &headless_results[1..] {
        assert!(
            reply.ends_with("The headlessly reattached child replied."),
            "{reply}"
        );
    }
    let Answer::Topic(value) = gui
        .query(Query::Topic(Topic::Controllers))
        .wait(STEP)
        .expect("controllers")
        .expect("controller topic")
    else {
        panic!("controller topic");
    };
    let TopicValue::Controllers(controllers) = *value else {
        panic!("controller rows");
    };
    assert!(
        !controllers.iter().any(|info| info.target.key == child.key),
        "finished headless tools must release their controller"
    );
    let adopted = run(
        &gui,
        Mutation::Launch(LaunchSpec {
            mux_name: Some("omp-tools-driven-child".into()),
            cwd: sb.proj(),
            session_path: None,
            label: "Driven child".into(),
            argv: vec!["omp".into()],
            role: SessionRole::Session,
            copy_of: None,
            view: SessionView::Chat,
        }),
    )
    .expect("frontend adopts child");
    let Done::Session(adopted, _) = adopted else {
        panic!("frontend session");
    };
    assert_eq!(adopted.key, child.key);
    assert_ne!(adopted.generation, child.generation);
    child = adopted;
    // A human's unsent composer draft must not be silently cleared by the tool.
    idle(&gui, &child);
    let (mut view, input) = gui
        .frontend(
            child.clone(),
            deck_service::gateway::protocol::ViewRole::Controlling,
        )
        .split();
    view.recv().expect("controlling view snapshot");
    // Actual human keystrokes, not an injected Review command: reattached native views
    // do not need terminal bracketed-paste negotiation to edit their composer.
    input
        .input(b"HUMAN-DRAFT keep this", deck_term::InputMeta::default())
        .expect("human typing");
    let ns = deck_host::Namespace::new(&sb.ns).expect("host namespace");
    wait_for("human draft in the native editor", STEP, || {
        deck_host::tsp_peek(&ns, "omp-tools-driven-child")
            .ok()
            .filter(|document| document.to_string().contains("HUMAN-DRAFT keep this"))
    });
    prompt(&gui, &opener, "DRAFT-MARK try sending over a human draft");
    let refusal = result(&model, "DRAFT-MARK");
    assert!(
        refusal.contains(
            "The session has unsent text in its composer; ask the user to send or clear it"
        ),
        "{refusal}"
    );
    assert!(
        model
            .find(0, |req| fm::last_user_text(req)
                .contains("MUST-NOT-BE-SENT"))
            .is_none(),
        "refused prompt must never reach the child model"
    );

    run(&gui, Mutation::Kill(child)).expect("kill child");
    prompt(
        &gui,
        &opener,
        "STALE-MARK send using the killed child's handle",
    );
    let stale = result(&model, "STALE-MARK");
    assert!(
        stale.contains("That session has exited or switched conversations; open a new one"),
        "{stale}"
    );
    run(&gui, Mutation::Kill(opener)).expect("stop opener");
}

const BACKGROUND_REPLY: &str = "BG-CHILD-REPLY: the slow child's complete result.";

fn background_script(req: &Value) -> Reply {
    if !fm::has_tools(req) {
        return Reply::text("Fake title");
    }
    let all = fm::messages(req)
        .iter()
        .map(fm::text)
        .collect::<Vec<_>>()
        .join("\n");
    if all.contains("Background wait:") && all.contains(BACKGROUND_REPLY) {
        return Reply::text("Background result received.");
    }
    let prompt = fm::last_user_text(req);
    if prompt.contains("BG-CHILD-") {
        return Reply::slow_text(BACKGROUND_REPLY, Duration::from_secs(8));
    }
    let results = tool_texts(req);
    let busy = prompt.contains("BG-BUSY-MARK");
    match results.len() {
        0 => Reply::tool_call(
            "ompdeck_open_session",
            &json!({
                "prompt": if busy { "BG-CHILD-BUSY" } else { "BG-CHILD-IDLE" },
                "title": if busy { "Background busy child" } else { "Background idle child" },
                "placement": "tab"
            }),
        ),
        1 => Reply::tool_call(
            "ompdeck_wait_session",
            &json!({
                "session": session_handle(req), "background": true
            }),
        ),
        2 if busy => Reply::tool_call(
            "bash",
            &json!({
                "command": "while [ ! -f bg-child-started ]; do sleep 0.05; done; printf started > bg-busy-started; sleep 16; printf done > bg-busy-done; printf BG-BUSY-TOOL-DONE"
            }),
        ),
        _ => Reply::text("The orchestrator finished its own work."),
    }
}

fn exercise_background_wait_idle_and_busy_delivery() {
    let Some(_omp) = which(if cfg!(windows) { "omp.exe" } else { "omp" }) else {
        eprintln!("skipped: omp is not installed");
        return;
    };
    let config = format!("{OMP_CONFIG}modelRoles:\n  default: {}\n", fm::MODEL);
    let sb = Sandbox::new("bg", &config);
    sb.report_failures();
    let child_started = sb.proj().join("bg-child-started");
    let model = FakeModel::start(move |req| {
        // Coordinate with actual child generation, not variable process-startup latency.
        if fm::has_tools(req) && fm::last_user_text(req).contains("BG-CHILD-BUSY") {
            std::fs::write(&child_started, "started").expect("child model gate");
        }
        background_script(req)
    });
    std::fs::write(sb.agent().join("models.yml"), model.models_yml()).expect("models.yml");
    let gui = sb.client();
    wait_for("GUI gateway link", STEP, || {
        matches!(gui.link(), Link::Up(_)).then_some(())
    });
    for (name, mark, busy) in [
        ("idle", "BG-IDLE-MARK", false),
        ("busy", "BG-BUSY-MARK", true),
    ] {
        let opener = launch(&gui, &sb, name, mark);
        let (request, child) = wait_for("background child launch", TURN, || {
            gui.events().into_iter().find_map(|event| match event {
                ClientEvent::AgentOpenSession {
                    request,
                    opener: target,
                    cwd,
                    prompt,
                    title,
                    ..
                } if target == opener => {
                    let done = run(
                        &gui,
                        Mutation::Launch(LaunchSpec {
                            mux_name: Some(format!("omp-tools-background-{name}")),
                            cwd,
                            session_path: None,
                            label: title.expect("child title"),
                            argv: vec!["omp".into(), prompt.expect("child prompt")],
                            role: SessionRole::Session,
                            copy_of: None,
                            view: SessionView::Chat,
                        }),
                    )
                    .expect("launch background child");
                    let Done::Session(child, SessionView::Chat) = done else {
                        panic!("launch: {done:?}")
                    };
                    Some((request, child))
                }
                _ => None,
            })
        });
        run(
            &gui,
            Mutation::AgentToolDone {
                request,
                target: opener.clone(),
                ok: true,
                text: "Opened background child.".into(),
                opened: Some(child.clone()),
            },
        )
        .expect("complete open-session tool");
        let registered = wait_for("immediate background wait result", TURN, || {
            model.find(0, |req| {
                fm::last_user_text(req).contains(mark)
                    && tool_texts(req)
                        .iter()
                        .any(|text| text.contains("Watching") && text.contains("in the background"))
            })
        });
        assert!(
            tool_texts(&registered)
                .iter()
                .any(|text| text.contains(&session_handle(&registered)))
        );
        wait_for(
            "child is still working after watch registration",
            STEP,
            || {
                let Ok(Answer::Topic(value)) =
                    gui.query(Query::Topic(Topic::Controllers)).wait(STEP)?
                else {
                    return None;
                };
                let TopicValue::Controllers(controllers) = *value else {
                    return None;
                };
                controllers
                    .iter()
                    .any(|info| info.target == child && info.activity.is_busy())
                    .then_some(())
            },
        );
        if busy {
            wait_for("orchestrator slow tool starts", STEP, || {
                sb.proj().join("bg-busy-started").is_file().then_some(())
            });
            idle(&gui, &child);
            assert!(
                !sb.proj().join("bg-busy-done").exists(),
                "child result must land while the tool runs"
            );
        } else {
            idle(&gui, &opener);
        }
        let delivered = wait_for(
            "background result triggers the orchestrator's next model step",
            TURN,
            || {
                model.find(0, |req| {
                    let all = fm::messages(req)
                        .iter()
                        .map(fm::text)
                        .collect::<Vec<_>>()
                        .join("\n");
                    fm::has_tools(req)
                        && all.contains(mark)
                        && all.contains("Background wait:")
                        && all.contains(BACKGROUND_REPLY)
                })
            },
        );
        if busy {
            assert!(
                sb.proj().join("bg-busy-done").is_file(),
                "delivery must not abort the in-flight tool"
            );
            assert!(
                fm::messages(&delivered)
                    .iter()
                    .filter(|message| fm::role(message) == "tool")
                    .any(|message| fm::text(message).contains("BG-BUSY-TOOL-DONE")),
                "the next step receives both the completed tool and child reply: {delivered}"
            );
        }
        run(&gui, Mutation::Kill(child)).expect("stop background child");
        run(&gui, Mutation::Kill(opener)).expect("stop orchestrator");
    }
}

const FORWARD_FORM: &str = r#"<form><p>Forwarded color?</p><label><input type="radio" name="color" value="red"> red</label><label><input type="radio" name="color" value="blue"> blue</label></form>"#;
const FORWARD_ASK_REPLY: &str = "FWD-ASK-FINAL: blue from the orchestrator.";
const FORWARD_APPROVAL_REPLY: &str = "FWD-APPROVAL-FINAL: the approved command ran.";

fn conversation_text(req: &Value) -> String {
    fm::messages(req)
        .iter()
        .map(fm::text)
        .collect::<Vec<_>>()
        .join("\n")
}

fn forwarded_question_id(req: &Value) -> String {
    fm::messages(req)
        .iter()
        .rev()
        .filter(|message| fm::role(message) == "user")
        .find_map(|message| {
            let text = fm::text(message);
            text.split("question: \"")
                .nth(1)
                .and_then(|rest| rest.split('"').next())
                .map(str::to_owned)
        })
        .expect("forwarded question has its quoted id")
}

fn forward_script(req: &Value, approval_cwd: &std::path::Path) -> Reply {
    if !fm::has_tools(req) {
        return Reply::text("Fake title");
    }
    let prompt = fm::last_user_text(req);
    let results = tool_texts(req);
    if prompt.contains("FWD-CHILD-") {
        if results.is_empty() {
            return if prompt.contains("APPROVE") {
                Reply::tool_call(
                    "bash",
                    &json!({"command":"printf done > approved-once; printf FWD-EXEC-RAN"}),
                )
            } else {
                Reply::slow_tool_call("ask", &json!({"html":FORWARD_FORM}), Duration::from_secs(2))
            };
        }
        return Reply::slow_text(
            if prompt.contains("APPROVE") {
                FORWARD_APPROVAL_REPLY
            } else {
                FORWARD_ASK_REPLY
            },
            Duration::from_secs(2),
        );
    }
    if prompt.contains("FWD-PARENT-OFF CHECK") {
        return Reply::slow_text("Checking without forwarding.", Duration::from_secs(3));
    }
    let all = conversation_text(req);
    if all.contains("Background wait:") && all.contains(FORWARD_APPROVAL_REPLY) {
        return Reply::text("Background watch survived the forwarded approval.");
    }
    if prompt.contains("Question from") {
        let approve = all.contains("FWD-PARENT-APPROVE");
        if approve && results.len() == 1 {
            assert!(
                !approval_cwd.join("approved-once").exists(),
                "refused session grant must not execute"
            );
        }
        let args = if approve {
            json!({"session": session_handle(req), "question": forwarded_question_id(req),
                "decision": if results.is_empty() { "session" } else { "once" }})
        } else {
            json!({"session":session_handle(req), "question":forwarded_question_id(req),
                "fields":[{"choices":["2"]}]})
        };
        if results.is_empty() || (approve && results.len() == 1) {
            return Reply::slow_tool_call(
                "ompdeck_answer_question",
                &args,
                if approve {
                    Duration::ZERO
                } else {
                    Duration::from_secs(1)
                },
            );
        }
        if results.len() == if approve { 2 } else { 1 } {
            return Reply::tool_call(
                "ompdeck_wait_session",
                &json!({"session":session_handle(req), "timeout_seconds":60}),
            );
        }
        return Reply::text("Child question answered and final reply received.");
    }
    if results.is_empty() {
        let approve = prompt.contains("APPROVE");
        let off = prompt.contains("OFF");
        let mut args = json!({"prompt":if approve { "FWD-CHILD-APPROVE" } else { "FWD-CHILD-ASK" },
            "title":"Forwarded child", "placement":"tab"});
        if off {
            args["forward_questions"] = json!(false);
        }
        if approve {
            args["cwd"] = json!(approval_cwd);
        }
        return Reply::tool_call("ompdeck_open_session", &args);
    }
    if results.len() == 1 && prompt.contains("APPROVE") {
        return Reply::tool_call(
            "ompdeck_wait_session",
            &json!({"session":session_handle(req), "background":true}),
        );
    }
    Reply::text("Orchestrator is idle while its child asks.")
}

fn exercise_forwarded_child_questions() {
    let Some(_omp) = which(if cfg!(windows) { "omp.exe" } else { "omp" }) else {
        eprintln!("skipped: omp is not installed");
        return;
    };
    let config = format!("{OMP_CONFIG}modelRoles:\n  default: {}\n", fm::MODEL);
    let sb = Sandbox::new("fq", &config);
    sb.report_failures();
    let approval_cwd = sb.proj().join("approval-child");
    std::fs::create_dir_all(approval_cwd.join(".omp")).expect("isolated child settings");
    std::fs::write(
        approval_cwd.join(".omp/config.yml"),
        "tools:\n  approvalMode: always-ask\n",
    )
    .expect("child-only approval mode");
    let model = FakeModel::start(move |req| forward_script(req, &approval_cwd));
    std::fs::write(sb.agent().join("models.yml"), model.models_yml()).expect("models.yml");
    let gui = sb.client();
    wait_for("forwarding GUI link", STEP, || {
        matches!(gui.link(), Link::Up(_)).then_some(())
    });
    for (name, mark) in [
        ("ask", "FWD-PARENT-ASK"),
        ("approve", "FWD-PARENT-APPROVE"),
        ("off", "FWD-PARENT-OFF"),
    ] {
        let opener = launch(&gui, &sb, &format!("fwd-{name}"), mark);
        let child_mux = format!("omp-tools-fq-{name}");
        let (request, child) = wait_for("forwarding child launch", TURN, || {
            gui.events().into_iter().find_map(|event| match event {
                ClientEvent::AgentOpenSession {
                    request,
                    opener: target,
                    cwd,
                    prompt,
                    title,
                    ..
                } if target == opener => {
                    let done = run(
                        &gui,
                        Mutation::Launch(LaunchSpec {
                            mux_name: Some(child_mux.clone()),
                            cwd,
                            session_path: None,
                            label: title.expect("title"),
                            argv: vec!["omp".into(), prompt.expect("prompt")],
                            role: SessionRole::Session,
                            copy_of: None,
                            view: SessionView::Chat,
                        }),
                    )
                    .expect("launch asking child");
                    let Done::Session(child, SessionView::Chat) = done else {
                        panic!("{done:?}")
                    };
                    Some((request, child))
                }
                _ => None,
            })
        });
        run(
            &gui,
            Mutation::AgentToolDone {
                request,
                target: opener.clone(),
                ok: true,
                text: "Opened asking child.".into(),
                opened: Some(child.clone()),
            },
        )
        .expect("register child");
        assert_ne!(opener.key, child.key, "child must be a distinct process");
        if name == "ask" {
            run(
                &gui,
                Mutation::Close {
                    target: child.clone(),
                    kill_direct: false,
                },
            )
            .expect("close asking child's terminal controller");
        }
        if name == "off" {
            gui.subscribe(Topic::Live(child.key.clone()));
            let question = wait_for("non-forwarded child's pending form", TURN, || {
                let Ok(Answer::Topic(value)) = gui
                    .query(Query::Topic(Topic::Live(child.key.clone())))
                    .wait(STEP)?
                else {
                    return None;
                };
                let TopicValue::Live {
                    snapshot: Some(snapshot),
                    ..
                } = *value
                else {
                    return None;
                };
                snapshot
                    .pending_ui
                    .into_iter()
                    .find(|ui| ui.kind == deck_core::ctl::PendingUiKind::Form)
            });
            prompt(&gui, &opener, "FWD-PARENT-OFF CHECK");
            idle(&gui, &opener);
            assert!(matches!(
                run(
                    &gui,
                    Mutation::AnswerUi {
                        target: child.clone(),
                        request: question.id,
                        answer: deck_service::ui_answer::UiAnswer::Form {
                            fields: vec![deck_service::ui_answer::FormAnswer::Choice {
                                checked: vec![1],
                                other: None
                            }]
                        }
                    }
                )
                .expect("user answers non-forwarded form"),
                Done::Answered(true)
            ));
            idle(&gui, &child);
            assert!(
                model
                    .find(0, |req| fm::has_tools(req)
                        && conversation_text(req).contains(mark)
                        && conversation_text(req).contains("Question from"))
                    .is_none(),
                "forward_questions:false must never send a question message"
            );
        } else {
            let forwarded = wait_for("parent receives a full forwarded question", TURN, || {
                model.find(0, |req| {
                    let all = conversation_text(req);
                    fm::has_tools(req)
                        && all.contains(mark)
                        && all.contains("Question from")
                        && all.contains(if name == "ask" {
                            "Forwarded color?"
                        } else {
                            "FWD-EXEC-RAN"
                        })
                })
            });
            let question_id = forwarded_question_id(&forwarded);
            assert!(!question_id.is_empty());
            if name == "ask" {
                let Answer::Topic(value) = gui
                    .query(Query::Topic(Topic::Controllers))
                    .wait(STEP)
                    .expect("controllers")
                    .expect("controller rows")
                else {
                    panic!("controllers")
                };
                let TopicValue::Controllers(controllers) = *value else {
                    panic!("controllers")
                };
                assert!(
                    !controllers.iter().any(|info| info.target.key == child.key),
                    "forwarding without a tab must retain only ctl, not a terminal controller"
                );
            }
            wait_for("answer reaches the child's real tool", TURN, || {
                model.find(0, |req| {
                    fm::last_user_text(req).contains(if name == "ask" {
                        "FWD-CHILD-ASK"
                    } else {
                        "FWD-CHILD-APPROVE"
                    }) && tool_texts(req).iter().any(|text| {
                        text.contains(if name == "ask" {
                            "blue"
                        } else {
                            "FWD-EXEC-RAN"
                        })
                    })
                })
            });
            wait_for("parent wait receives the child's final reply", TURN, || {
                model.find(0, |req| {
                    let all = conversation_text(req);
                    fm::has_tools(req)
                        && all.contains(mark)
                        && all.contains(if name == "ask" {
                            FORWARD_ASK_REPLY
                        } else {
                            FORWARD_APPROVAL_REPLY
                        })
                        && (name != "approve"
                            || fm::messages(req).iter().any(|message| {
                                let text = fm::text(message);
                                fm::role(message) == "user"
                                    && text.contains("Background wait:")
                                    && text.contains(FORWARD_APPROVAL_REPLY)
                            }))
                })
            });
            if name == "approve" {
                assert!(
                    sb.proj().join("approval-child/approved-once").is_file(),
                    "Approve once must actually execute the child's command"
                );
                assert!(
                    model
                        .find(0, |req| conversation_text(req).contains(mark)
                            && fm::messages(req)
                                .iter()
                                .filter(|m| fm::role(m) == "tool")
                                .map(fm::text)
                                .any(|text| text.contains("once")
                                    && text.contains("deny")
                                    && !text.contains("Answered.")))
                        .is_some(),
                    "the attempted session grant must be refused before approving once"
                );
            }
        }
        run(
            &gui,
            Mutation::KillNames {
                names: vec![child_mux],
                wait: false,
            },
        )
        .expect("stop asking child");
        run(&gui, Mutation::Kill(opener)).expect("stop question orchestrator");
    }
}

#[test]
fn real_agent_tools_notify_list_review_and_require_a_window() {
    // Sandbox owns process-wide HOME/XDG variables: keep all scenarios in one test.
    exercise_notify_list_review_and_require_a_window();
    exercise_open_wait_send_follow_up_draft_and_stale_handle();
    exercise_background_wait_idle_and_busy_delivery();
    exercise_forwarded_child_questions();
}
