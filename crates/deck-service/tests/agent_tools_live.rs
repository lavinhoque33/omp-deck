//! Real omp extension tools through the gateway, driven by the scripted model.
//! Uses only a private HOME/XDG tree and test host/tmux namespace.

mod support;

use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
use std::sync::Arc;
use std::time::Duration;

use deck_core::config::SessionView;
use deck_service::controller::{Command, Delivery, SessionTarget};
use deck_service::gateway::client::{ClientEvent, GatewayClient, GatewayOptions, Link};
use deck_service::gateway::protocol::{
    Answer, Done, LaunchSpec, Mutation, OrchFinish, Outcome, Query, ReviewReply, Role, SessionRole,
    Topic, TopicValue, WorktreeFlow,
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
            agent_child: false,
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
                        agent_child: true,
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
    // Completion can reach the model before the input-tail grace and the next
    // Controllers publication. Progress subscriptions do not own this attachment.
    let release_deadline = std::time::Instant::now() + Duration::from_secs(10);
    wait_for(
        "finished headless tools release their controller",
        Duration::from_secs(10),
        || {
            let Ok(Answer::Topic(value)) = gui
                .query(Query::Topic(Topic::Controllers))
                .wait(release_deadline.saturating_duration_since(std::time::Instant::now()))?
            else {
                return None;
            };
            let TopicValue::Controllers(controllers) = *value else {
                return None;
            };
            (!controllers.iter().any(|info| info.target.key == child.key)).then_some(())
        },
    );
    let adopted = run(
        &gui,
        Mutation::Launch(LaunchSpec {
            agent_child: true,
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
                            agent_child: true,
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
                            agent_child: true,
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
            let forwarded = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                wait_for("parent receives a full forwarded question", TURN, || {
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
                })
            }))
            .unwrap_or_else(|error| {
                eprintln!(
                    "forwarded-question timeout: scenario={name}, mark={mark}, opener={opener:?}, child={child:?}, captured_requests={}",
                    model.count()
                );
                for (index, req) in model.since(0).into_iter().enumerate() {
                    eprintln!(
                        "==== forwarding model request {index}: has_tools={}, last_user={:?} ====\n{:#}\n==== end forwarding model request {index} ====",
                        fm::has_tools(&req),
                        fm::last_user_text(&req),
                        json!(fm::messages(&req))
                    );
                }
                std::panic::resume_unwind(error);
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

/// Make the selected real binary visible to both fixture admission and Omp::detect.
fn test_omp_override() -> Option<tempfile::TempDir> {
    let binary = std::env::var_os("OMPDECK_TEST_OMP")?;
    let binary = std::fs::canonicalize(binary).expect("OMPDECK_TEST_OMP must name a real binary");
    assert!(binary.is_file(), "OMPDECK_TEST_OMP must name a file");
    let dir = tempfile::tempdir().expect("omp override directory");
    let target = dir
        .path()
        .join(if cfg!(windows) { "omp.exe" } else { "omp" });
    #[cfg(unix)]
    std::os::unix::fs::symlink(&binary, &target).expect("link selected omp");
    #[cfg(windows)]
    std::fs::copy(&binary, &target).expect("copy selected omp");
    let mut paths = vec![dir.path().to_path_buf()];
    paths.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    // SAFETY: the sole test calls this before any fixture/model/gateway threads start.
    unsafe {
        std::env::set_var("PATH", std::env::join_paths(paths).expect("override PATH"));
    }
    Some(dir)
}

fn fixture_git(repo: &Path, args: &[&str]) -> String {
    let output = ProcessCommand::new("git")
        .current_dir(repo)
        .args(args)
        .output()
        .expect("run sandbox git");
    assert!(
        output.status.success(),
        "git {args:?} in {}: {}",
        repo.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("git UTF-8 output")
        .trim()
        .to_owned()
}

const WORKTREE_BRANCH: &str = "wt/agent-live";
const WORKTREE_REPORT: &str = "WT-REPORT: committed the child change; now validating it.";
const WORKTREE_CURRENT: &str = "Validate the linked checkout";

fn worktree_script(req: &Value, primary: &Path, registered: &Path, release: &Path) -> Reply {
    if !fm::has_tools(req) {
        return Reply::text("Fake title");
    }
    let prompt = fm::last_user_text(req);
    let results = tool_texts(req);
    let quote = |path: &Path| deck_core::worktree::shell_quote(&path.to_string_lossy());
    let (name, args) = if prompt.contains("Finish this worktree:") {
        if !results.is_empty() {
            assert!(results[0].contains("WT-MERGE-RAN"), "{results:?}");
            return Reply::text("WT-FINISH-MERGED: committed work merged into main.");
        }
        (
            "bash",
            json!({"command": format!(
                "git -C {} merge --no-ff -m 'merge live agent child' {} && printf WT-MERGE-RAN",
                quote(primary), WORKTREE_BRANCH
            )}),
        )
    } else if prompt.contains("WT-CHILD-WORK") {
        match results.len() {
            0 => (
                "bash",
                json!({"command": format!(
                    "while [ ! -f {} ]; do sleep 0.05; done; pwd > child-cwd.txt && printf 'real child work\\n' > child-change.txt && git add child-cwd.txt child-change.txt && git commit -qm 'child worktree change' && printf WT-CHILD-COMMITTED",
                    quote(registered)
                )}),
            ),
            1 => {
                assert!(results[0].contains("WT-CHILD-COMMITTED"), "{results:?}");
                (
                    "todo",
                    json!({"op": "init", "list": [{
                        "phase": "Worktree delivery",
                        "items": ["Commit the child change", WORKTREE_CURRENT, "Summarize the result"]
                    }]}),
                )
            }
            2 => (
                "todo",
                json!({"op": "done", "task": "Commit the child change"}),
            ),
            3 => ("todo", json!({"op": "start", "task": WORKTREE_CURRENT})),
            4 => ("ompdeck_report", json!({"text": WORKTREE_REPORT})),
            5 => {
                assert!(results[4].starts_with("Reported to "), "{results:?}");
                (
                    "bash",
                    json!({"command": format!(
                        "while [ ! -f {} ]; do sleep 0.05; done; printf WT-PROGRESS-RELEASED",
                        quote(release)
                    )}),
                )
            }
            6 => {
                assert!(results[5].contains("WT-PROGRESS-RELEASED"), "{results:?}");
                ("todo", json!({"op": "done", "task": WORKTREE_CURRENT}))
            }
            7 => (
                "todo",
                json!({"op": "done", "task": "Summarize the result"}),
            ),
            _ => return Reply::text("WT-CHILD-READY: committed, validated, and ready for Finish."),
        }
    } else if prompt.contains("WT-CHILD-AFTER") {
        if !results.is_empty() {
            assert!(
                results[0].contains(&*primary.to_string_lossy()),
                "{results:?}"
            );
            return Reply::text("WT-RELOCATED-REPLY: the same child now runs in primary.");
        }
        ("bash", json!({"command": "pwd"}))
    } else if prompt.contains("WT-PARENT-OPEN") {
        assert!(
            !fm::tool_names(req).contains(&"ompdeck_report"),
            "ordinary parent launches must not expose child reports"
        );
        if !results.is_empty() {
            return Reply::text("Worktree child opened.");
        }
        (
            "ompdeck_open_session",
            json!({
                "prompt": "WT-CHILD-WORK", "title": "Worktree live child", "placement": "tab",
                "worktree": {"branch": WORKTREE_BRANCH}
            }),
        )
    } else if prompt.contains("WT-PARENT-LIST") {
        if !results.is_empty() {
            return Reply::text("Progress row received.");
        }
        ("ompdeck_sessions", json!({}))
    } else if prompt.contains("WT-PARENT-FINISH") {
        if !results.is_empty() {
            return Reply::text("Finish injection acknowledged.");
        }
        (
            "ompdeck_finish_worktree",
            json!({"session": session_handle(req)}),
        )
    } else if prompt.contains("WT-PARENT-AFTER") {
        match results.len() {
            0 => (
                "ompdeck_send_prompt",
                json!({"session": session_handle(req), "prompt": "WT-CHILD-AFTER"}),
            ),
            1 => (
                "ompdeck_wait_session",
                json!({"session": session_handle(req), "timeout_seconds": 60}),
            ),
            2 => ("ompdeck_sessions", json!({})),
            _ => return Reply::text("Same handle survived worktree retirement."),
        }
    } else {
        // Reports and finish outcomes arrive as real aside messages.
        return Reply::text("Child message received.");
    };
    assert!(
        fm::tool_names(req).contains(&name),
        "extension must advertise {name}"
    );
    Reply::tool_call(name, &args)
}

fn exercise_worktree_report_progress_and_finish() {
    if which(if cfg!(windows) { "omp.exe" } else { "omp" }).is_none()
        || which(if cfg!(windows) { "git.exe" } else { "git" }).is_none()
    {
        eprintln!("skipped: omp or git is not installed");
        return;
    }
    let config = format!("{OMP_CONFIG}modelRoles:\n  default: {}\n", fm::MODEL);
    let sb = Sandbox::new("wr", &config);
    sb.report_failures();
    let managed = sb.root.join("home/worktrees");
    // SAFETY: previous fixtures have dropped their model/client/owner; set these before
    // starting this scenario's model or gateway threads.
    unsafe {
        std::env::set_var("OMP_WORKTREE_DIR", &managed);
        std::env::set_var("GIT_CONFIG_GLOBAL", sb.root.join("home/.gitconfig"));
        std::env::set_var("GIT_CONFIG_NOSYSTEM", "1");
    }
    std::fs::write(
        sb.root.join("home/.gitconfig"),
        "[user]\n\tname = Deck Test\n\temail = deck@test.invalid\n[commit]\n\tgpgsign = false\n",
    )
    .expect("sandbox git config");
    let primary = sb.proj();
    fixture_git(&primary, &["init", "-q", "-b", "main"]);
    std::fs::write(primary.join("initial.txt"), "initial\n").expect("initial fixture file");
    fixture_git(&primary, &["add", "initial.txt"]);
    fixture_git(&primary, &["commit", "-qm", "initial fixture commit"]);
    let initial_tip = fixture_git(&primary, &["rev-parse", "HEAD"]);
    let registered = sb.root.join("child-registered");
    let release = sb.root.join("release-progress");
    let model = {
        let primary = primary.clone();
        let registered = registered.clone();
        let release = release.clone();
        FakeModel::start(move |req| worktree_script(req, &primary, &registered, &release))
    };
    std::fs::write(sb.agent().join("models.yml"), model.models_yml()).expect("models.yml");
    let gui = sb.client();
    wait_for("worktree GUI link", STEP, || {
        matches!(gui.link(), Link::Up(_)).then_some(())
    });
    // No Topic::Live subscription: Controllers must mirror todos on its own ctl link.
    let opener = launch(&gui, &sb, "worktree-parent", "WT-PARENT-OPEN");
    let (request, child, cwd) = wait_for("new linked worktree child launch", TURN, || {
        gui.events().into_iter().find_map(|event| match event {
            ClientEvent::AgentOpenSession {
                request,
                opener: target,
                cwd,
                prompt,
                title,
                ..
            } if target == opener => {
                assert!(
                    cwd.starts_with(&managed),
                    "worktree escaped sandbox: {cwd:?}"
                );
                assert_ne!(cwd, primary);
                let worktree = deck_core::worktree::resolve(&cwd).expect("real linked worktree");
                assert_eq!(worktree.primary, primary);
                assert_eq!(worktree.branch.as_deref(), Some(WORKTREE_BRANCH));
                assert!(
                    cwd.join(".git").is_file(),
                    "linked worktree has a .git file"
                );
                let done = run(
                    &gui,
                    Mutation::Launch(LaunchSpec {
                        agent_child: true,
                        mux_name: Some("omp-tools-worktree-child".into()),
                        cwd: cwd.clone(),
                        session_path: None,
                        label: title.expect("worktree child title"),
                        argv: vec!["omp".into(), prompt.expect("worktree child prompt")],
                        role: SessionRole::Session,
                        copy_of: None,
                        view: SessionView::Chat,
                    }),
                )
                .expect("GUI launches worktree child");
                let Done::Session(child, SessionView::Chat) = done else {
                    panic!("worktree child launch: {done:?}");
                };
                Some((request, child, cwd))
            }
            _ => None,
        })
    });
    let Answer::Topic(settings) = gui
        .query(Query::Topic(Topic::Settings))
        .wait(STEP)
        .expect("settings response")
        .expect("settings query")
    else {
        panic!("settings response");
    };
    let TopicValue::Settings(settings) = *settings else {
        panic!("settings topic");
    };
    assert_eq!(
        settings.config.worktree_bases.get(&cwd).map(String::as_str),
        Some("main")
    );
    assert_eq!(fixture_git(&cwd, &["rev-parse", "main"]), initial_tip);
    run(
        &gui,
        Mutation::AgentToolDone {
            request,
            target: opener.clone(),
            ok: true,
            text: "Opened Worktree live child.".into(),
            opened: Some(child.clone()),
        },
    )
    .expect("register real worktree child");
    std::fs::write(&registered, "registered").expect("release registered child");
    let opened = result(&model, "WT-PARENT-OPEN");
    let open_request = wait_for("opener receives child handle", TURN, || {
        model.find(0, |req| {
            fm::last_user_text(req).contains("WT-PARENT-OPEN") && !tool_texts(req).is_empty()
        })
    });
    let handle = session_handle(&open_request);
    assert!(opened.contains("Worktree: "), "{opened}");
    assert!(opened.contains(&*cwd.to_string_lossy()), "{opened}");
    assert!(
        opened.contains(WORKTREE_BRANCH) && opened.contains("merges into main"),
        "{opened}"
    );
    let report = wait_for("child report delivered as an opener aside", TURN, || {
        model.find(0, |req| {
            fm::has_tools(req)
                && fm::messages(req).iter().any(|message| {
                    let text = fm::text(message);
                    fm::role(message) == "user"
                        && text.contains("Report from Worktree live child")
                        && text.contains(WORKTREE_REPORT)
                        && text.contains(&handle)
                })
        })
    });
    assert!(
        conversation_text(&report).contains("WT-PARENT-OPEN"),
        "report belongs to opener"
    );
    wait_for("controller mirrors native todo mutation", TURN, || {
        let Ok(Answer::Topic(value)) = gui.query(Query::Topic(Topic::Controllers)).wait(STEP)?
        else {
            return None;
        };
        let TopicValue::Controllers(controllers) = *value else {
            return None;
        };
        let info = controllers.into_iter().find(|info| info.target == child)?;
        let progress = info.progress?;
        (progress.counts() == (1, 3)
            && progress.current().is_some_and(|task| {
                task.content == WORKTREE_CURRENT
                    && task.status == deck_core::ctl::TodoStatus::InProgress
            }))
        .then_some(())
    });
    assert_eq!(
        std::fs::read_to_string(cwd.join("child-cwd.txt"))
            .expect("child pwd")
            .trim(),
        cwd.to_string_lossy()
    );
    assert_eq!(fixture_git(&cwd, &["status", "--porcelain"]), "");
    assert_ne!(fixture_git(&cwd, &["rev-parse", "HEAD"]), initial_tip);
    assert!(
        !primary.join("child-change.txt").exists(),
        "child change is isolated until merge"
    );
    prompt(&gui, &opener, "WT-PARENT-LIST");
    let listed = result(&model, "WT-PARENT-LIST");
    let row = listed
        .lines()
        .find(|row| row.contains(&format!("handle: {handle}")))
        .expect("child session row");
    assert!(row.contains("worktree wt/agent-live → main"), "{row}");
    assert!(
        row.contains("todos: 1/3") && row.contains(&format!("now: {WORKTREE_CURRENT}")),
        "{row}"
    );
    assert!(
        row.contains("last report:") && row.contains(WORKTREE_REPORT),
        "{row}"
    );
    std::fs::write(&release, "continue").expect("release child validation tool");
    wait_for("child completes its native todo tools", TURN, || {
        model.find(0, |req| {
            fm::last_user_text(req).contains("WT-CHILD-WORK") && tool_texts(req).len() == 8
        })
    });
    idle(&gui, &child);
    let before_move = wait_for("child conversation before relocation", STEP, || {
        let Ok(Answer::Topic(value)) = gui.query(Query::Topic(Topic::Controllers)).wait(STEP)?
        else {
            return None;
        };
        let TopicValue::Controllers(controllers) = *value else {
            return None;
        };
        let info = controllers.into_iter().find(|info| info.target == child)?;
        assert_eq!(info.progress.as_ref().map(|p| p.counts()), Some((3, 3)));
        info.session_path
    });
    prompt(&gui, &opener, "WT-PARENT-FINISH");
    let started = result(&model, "WT-PARENT-FINISH");
    assert!(started.starts_with("Finish started:"), "{started}");
    assert!(
        started.contains(WORKTREE_BRANCH) && started.contains("main"),
        "{started}"
    );
    wait_for(
        "actual Finish prompt reaches the worktree child",
        TURN,
        || {
            model.find(0, |req| {
                fm::has_tools(req)
                    && fm::last_user_text(req).contains("Finish this worktree:")
                    && fm::last_user_text(req).contains(WORKTREE_BRANCH)
                    && fm::last_user_text(req).contains("Do not delete the worktree")
            })
        },
    );
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        wait_for(
            "opener receives real worktree removal outcome",
            Duration::from_secs(150),
            || {
                model.find(0, |req| {
                    fm::has_tools(req)
                        && fm::messages(req).iter().any(|message| {
                            let text = fm::text(message);
                            fm::role(message) == "user"
                                && text.contains("Worktree finish: Worktree live child")
                                && text.contains(&handle)
                                && text.to_lowercase().contains("removed")
                                && (text.contains("~/proj")
                                    || text.contains(&*primary.to_string_lossy()))
                        })
                })
            },
        );
    }));
    if let Err(error) = outcome {
        eprintln!(
            "finish controller state: {:?}",
            gui.query(Query::Topic(Topic::Controllers)).wait(STEP)
        );
        if let Some(req) = model.find(0, |req| {
            fm::has_tools(req)
                && fm::last_user_text(req).contains("Finish this worktree:")
                && !tool_texts(req).is_empty()
        }) {
            eprintln!("finish model tool results: {:?}", tool_texts(&req));
        }
        if let Some(req) = model.find(0, |req| {
            fm::messages(req)
                .iter()
                .any(|message| fm::text(message).contains("Worktree finish:"))
        }) {
            for message in fm::messages(&req) {
                let text = fm::text(message);
                if text.contains("Worktree finish:") {
                    eprintln!("finish opener aside: {text}");
                }
            }
        }
        std::panic::resume_unwind(error);
    }
    assert!(!cwd.exists(), "gateway must remove the finished worktree");
    assert!(
        !fixture_git(&primary, &["worktree", "list", "--porcelain"])
            .contains(&*cwd.to_string_lossy()),
        "worktree remains registered"
    );
    assert!(
        !fixture_git(&primary, &["branch", "--list", WORKTREE_BRANCH]).contains(WORKTREE_BRANCH),
        "merged child branch must be removed"
    );
    assert_eq!(fixture_git(&primary, &["status", "--porcelain"]), "");
    assert_eq!(
        std::fs::read_to_string(primary.join("child-change.txt"))
            .expect("actual merged child file"),
        "real child work\n"
    );
    assert!(
        fixture_git(&primary, &["log", "-1", "--format=%s"]).contains("merge live agent child")
    );
    let relocated = wait_for("same live child relocates to primary", STEP, || {
        let Ok(Answer::Topic(value)) = gui.query(Query::Topic(Topic::Controllers)).wait(STEP)?
        else {
            return None;
        };
        let TopicValue::Controllers(controllers) = *value else {
            return None;
        };
        let info = controllers
            .into_iter()
            .find(|info| info.target.key == child.key)?;
        (info.cwd == primary
            && info.exit_code.is_none()
            && info
                .session_path
                .as_ref()
                .is_some_and(|path| path != &before_move))
        .then_some(info.target)
    });
    prompt(&gui, &opener, "WT-PARENT-AFTER");
    let after = wait_for(
        "same handle works after sanctioned relocation",
        TURN,
        || {
            model.find(0, |req| {
                fm::last_user_text(req).contains("WT-PARENT-AFTER") && tool_texts(req).len() == 3
            })
        },
    );
    assert_eq!(session_handle(&after), handle);
    let results = tool_texts(&after);
    assert!(results[0].contains("Sent to"), "{results:?}");
    assert!(
        results[1].contains("finished (completed)")
            && results[1].ends_with("WT-RELOCATED-REPLY: the same child now runs in primary."),
        "{results:?}"
    );
    assert!(
        results[2]
            .lines()
            .any(|row| row.contains(&format!("handle: {handle}"))
                && !row.contains("worktree wt/agent-live")),
        "{results:?}"
    );
    run(&gui, Mutation::Kill(relocated)).expect("stop relocated worktree child");
    run(&gui, Mutation::Kill(opener)).expect("stop worktree opener");
}

const FAN_BRANCHES: [&str; 2] = ["wt/fan-a", "wt/fan-b"];
const FAN_TITLES: [&str; 2] = ["Fan A", "Fan B"];
const FAN_REPLIES: [&str; 2] = [
    "FAN-A-DONE: the first isolated change is committed.",
    "FAN-B-DONE: the second isolated change is committed.",
];

fn session_handles(req: &Value) -> Vec<String> {
    let mut handles = Vec::new();
    for message in fm::messages(req).iter().filter(|m| fm::role(m) == "tool") {
        for word in fm::text(message).split_whitespace() {
            let word = word.trim_end_matches(':');
            if word.len() == 10
                && word.starts_with("s-")
                && word[2..].bytes().all(|byte| byte.is_ascii_hexdigit())
                && !handles.iter().any(|handle| handle == word)
            {
                handles.push(word.to_owned());
            }
        }
    }
    handles
}

fn session_file(client: &GatewayClient, target: &SessionTarget) -> PathBuf {
    wait_for("persisted live session identity", TURN, || {
        let Ok(Answer::Topic(value)) = client.query(Query::Topic(Topic::Controllers)).wait(STEP)?
        else {
            return None;
        };
        let TopicValue::Controllers(controllers) = *value else {
            return None;
        };
        controllers
            .into_iter()
            .find(|info| &info.target == target)?
            .session_path
            .filter(|path| path.is_file())
    })
}

fn session_entries(file: &Path) -> Vec<Value> {
    std::fs::read_to_string(file)
        .expect("read sandbox session")
        .lines()
        // A live append can leave only the final line incomplete.
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

fn orchestration_script(req: &Value, primary: &Path, markers: &Path) -> Reply {
    if !fm::has_tools(req) {
        return Reply::text("Fake title");
    }
    let prompt = fm::last_user_text(req);
    let results = tool_texts(req);
    let quote = |path: &Path| deck_core::worktree::shell_quote(&path.to_string_lossy());
    let (name, args) = if prompt.contains("Finish this worktree:") {
        let index = FAN_BRANCHES
            .iter()
            .position(|branch| prompt.contains(branch))
            .expect("finish prompt identifies a fan-out branch");
        if !results.is_empty() {
            assert!(results[0].contains("FAN-MERGE-RAN"), "{results:?}");
            return Reply::text("FAN-FINISH-DONE: updated from main and merged.");
        }
        let lane = if index == 0 { "a" } else { "b" };
        let hold = if index == 0 {
            format!(
                "touch {} && for ((i=0;i<2400;i++)); do [ -f {} ] && break; sleep 0.05; done; test -f {} && ",
                quote(&markers.join("first-entered")),
                quote(&markers.join("release-first")),
                quote(&markers.join("release-first"))
            )
        } else {
            format!("touch {} && ", quote(&markers.join("second-entered")))
        };
        (
            "bash",
            json!({"command": format!(
                "mkdir {} && printf '{lane}-start\\n' >> {} && {hold}git merge --no-edit main && git -C {} merge --no-ff -m 'merge fan {lane}' {} && printf '{lane}-end\\n' >> {} && rmdir {} && printf FAN-MERGE-RAN",
                quote(&markers.join("merge-lock")),
                quote(&markers.join("merge-events")),
                quote(primary),
                FAN_BRANCHES[index],
                quote(&markers.join("merge-events")),
                quote(&markers.join("merge-lock"))
            )}),
        )
    } else if prompt.contains("FAN-CHILD-A") || prompt.contains("FAN-CHILD-B") {
        assert!(
            fm::tool_names(req).contains(&"ompdeck_report"),
            "the GUI bridge must launch each fan-out session as an agent child"
        );
        let index = usize::from(prompt.contains("FAN-CHILD-B"));
        if !results.is_empty() {
            assert!(results[0].contains("FAN-CHILD-COMMITTED"), "{results:?}");
            return Reply::slow_text(FAN_REPLIES[index], Duration::from_secs(2));
        }
        let lane = if index == 0 { "a" } else { "b" };
        (
            "bash",
            json!({"command": format!(
                "for ((i=0;i<2400;i++)); do [ -f {} ] && break; sleep 0.05; done; test -f {} && printf 'isolated {lane}\\n' > fan-{lane}.txt && git add fan-{lane}.txt && git commit -qm 'fan {lane} work' && printf FAN-CHILD-COMMITTED",
                quote(&markers.join("children-registered")),
                quote(&markers.join("children-registered"))
            )}),
        )
    } else if prompt.contains("FAN-PARENT-OPEN") {
        if !results.is_empty() {
            return Reply::text("Fan-out opened; waiting for its single combined result.");
        }
        (
            "ompdeck_fan_out",
            json!({
                "tasks": [
                    {"prompt": "FAN-CHILD-A", "title": FAN_TITLES[0], "branch": FAN_BRANCHES[0]},
                    {"prompt": "FAN-CHILD-B", "title": FAN_TITLES[1], "branch": FAN_BRANCHES[1]}
                ],
                "worktree": true,
                "timeout_seconds": 120
            }),
        )
    } else if prompt.contains("FAN-PARENT-FINISH") {
        let handles = session_handles(req);
        assert_eq!(handles.len(), 2, "each fan-out child has its own handle");
        match results.len() {
            0 | 1 => (
                "ompdeck_finish_worktree",
                json!({"session": handles[results.len()]}),
            ),
            _ => return Reply::text("Both queued finishes were acknowledged."),
        }
    } else if prompt.contains("CHECKPOINT-READ") {
        if !results.is_empty() {
            assert!(results[0].contains("before checkpoint"), "{results:?}");
            return Reply::text("Read-only turn completed without a checkpoint.");
        }
        ("read", json!({"path": "checkpoint-file.txt"}))
    } else if prompt.contains("CHECKPOINT-EDIT") {
        if !results.is_empty() {
            assert!(
                results[0].contains("CHECKPOINT-CAPTURED-BEFORE-EDIT"),
                "{results:?}"
            );
            return Reply::text("Checkpoint edit turn completed.");
        }
        let session_id = prompt
            .split_whitespace()
            .find_map(|word| word.strip_prefix("session="))
            .expect("checkpoint prompt includes the actual session id");
        assert!(
            session_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-'),
            "safe git ref component"
        );
        (
            "bash",
            json!({"command": format!(
                "ref=$(git for-each-ref --sort=-refname --format='%(refname)' refs/ompdeck/checkpoints/{session_id}/ | head -n 1); test -n \"$ref\" && test \"$(git show \"$ref:checkpoint-file.txt\")\" = 'before checkpoint' && printf 'after checkpoint\\n' > checkpoint-file.txt && printf CHECKPOINT-CAPTURED-BEFORE-EDIT"
            )}),
        )
    } else {
        // Only the real extension's Deliver messages produce these model requests.
        return Reply::text("Orchestration aside received.");
    };
    assert!(
        fm::tool_names(req).contains(&name),
        "extension must advertise {name}"
    );
    Reply::tool_call(name, &args)
}

fn exercise_fan_out_merge_queue_and_checkpoints() {
    if which(if cfg!(windows) { "omp.exe" } else { "omp" }).is_none()
        || which(if cfg!(windows) { "git.exe" } else { "git" }).is_none()
    {
        eprintln!("skipped: omp or git is not installed");
        return;
    }
    let config = format!("{OMP_CONFIG}modelRoles:\n  default: {}\n", fm::MODEL);
    let sb = Sandbox::new("oq", &config);
    sb.report_failures();
    let managed = sb.root.join("home/worktrees");
    // SAFETY: the preceding scenario has dropped all clients/model/owner. All paths
    // are private fixture paths, set before this scenario starts any threads.
    unsafe {
        std::env::set_var("OMP_WORKTREE_DIR", &managed);
        std::env::set_var("GIT_CONFIG_GLOBAL", sb.root.join("home/.gitconfig"));
        std::env::set_var("GIT_CONFIG_NOSYSTEM", "1");
    }
    std::fs::write(
        sb.root.join("home/.gitconfig"),
        "[user]\n\tname = Deck Test\n\temail = deck@test.invalid\n[commit]\n\tgpgsign = false\n",
    )
    .expect("sandbox git config");
    let primary = sb.proj();
    fixture_git(&primary, &["init", "-q", "-b", "main"]);
    std::fs::write(primary.join("initial.txt"), "initial\n").expect("initial fixture file");
    fixture_git(&primary, &["add", "initial.txt"]);
    fixture_git(&primary, &["commit", "-qm", "initial fixture commit"]);
    let initial_tip = fixture_git(&primary, &["rev-parse", "HEAD"]);
    let markers = sb.root.join("orchestration-markers");
    std::fs::create_dir(&markers).expect("private fixture markers outside git");
    let model = {
        let primary = primary.clone();
        let markers = markers.clone();
        FakeModel::start(move |req| orchestration_script(req, &primary, &markers))
    };
    std::fs::write(sb.agent().join("models.yml"), model.models_yml()).expect("models.yml");
    let gui = sb.client();
    wait_for("orchestration GUI link", STEP, || {
        matches!(gui.link(), Link::Up(_)).then_some(())
    });
    gui.subscribe(Topic::Orchestration);
    gui.subscribe(Topic::Worktrees(primary.clone()));
    let opener = launch(&gui, &sb, "fan-parent", "FAN-PARENT-OPEN");
    let mut children = Vec::new();
    let mut open_requests = Vec::new();
    wait_for(
        "both fan-out GUI opens individually registered",
        TURN,
        || {
            // Drain the whole batch: find_map would discard a concurrent second open.
            for event in gui.events() {
                if let ClientEvent::AgentOpenSession {
                    request,
                    opener: target,
                    cwd,
                    prompt,
                    title,
                    model,
                    ..
                } = event
                {
                    assert_eq!(target, opener);
                    assert!(model.is_none());
                    assert!(
                        !open_requests.contains(&request),
                        "distinct per-child open ids"
                    );
                    let title = title.expect("fan-out child title");
                    let index = FAN_TITLES
                        .iter()
                        .position(|expected| *expected == title)
                        .expect("expected fan-out task");
                    assert!(
                        !children.iter().any(|(i, _, _)| *i == index),
                        "task opened once"
                    );
                    assert!(cwd.starts_with(&managed), "worktree escaped sandbox HOME");
                    let worktree =
                        deck_core::worktree::resolve(&cwd).expect("real linked checkout");
                    assert_eq!(worktree.primary, primary);
                    assert_eq!(worktree.branch.as_deref(), Some(FAN_BRANCHES[index]));
                    assert_eq!(fixture_git(&cwd, &["rev-parse", "main"]), initial_tip);
                    let done = run(
                        &gui,
                        Mutation::Launch(LaunchSpec {
                            agent_child: true,
                            mux_name: Some(format!("omp-tools-fan-child-{index}")),
                            cwd: cwd.clone(),
                            session_path: None,
                            label: title.clone(),
                            argv: vec!["omp".into(), prompt.expect("fan-out initial prompt")],
                            role: SessionRole::Session,
                            copy_of: None,
                            view: SessionView::Chat,
                        }),
                    )
                    .expect("GUI launches fan-out child");
                    let Done::Session(child, SessionView::Chat) = done else {
                        panic!("fan-out child launch: {done:?}");
                    };
                    run(
                        &gui,
                        Mutation::AgentToolDone {
                            request,
                            target: opener.clone(),
                            ok: true,
                            text: format!("Opened {title}."),
                            opened: Some(child.clone()),
                        },
                    )
                    .expect("acknowledge this individual fan-out open");
                    open_requests.push(request);
                    children.push((index, child, cwd));
                }
            }
            (children.len() == 2).then_some(())
        },
    );
    children.sort_by_key(|(index, _, _)| *index);
    assert_ne!(children[0].1.key, children[1].1.key);
    let opened = result(&model, "FAN-PARENT-OPEN");
    assert!(
        opened.contains("Their combined result arrives as one message when all have finished."),
        "{opened}"
    );
    let open_request = model
        .find(0, |req| {
            fm::last_user_text(req).contains("FAN-PARENT-OPEN") && !tool_texts(req).is_empty()
        })
        .expect("real fan-out tool result");
    let handles = session_handles(&open_request);
    assert_eq!(handles.len(), 2, "{opened}");
    for index in 0..2 {
        let row = opened
            .lines()
            .find(|line| line.starts_with(&format!("{}.", index + 1)))
            .expect("one result line per task");
        assert!(
            row.contains(&handles[index])
                && row.contains(FAN_TITLES[index])
                && row.contains(FAN_BRANCHES[index]),
            "{row}"
        );
    }
    let fan_out = wait_for("orchestration records both fan-out children", TURN, || {
        let Ok(Answer::Topic(value)) = gui.query(Query::Topic(Topic::Orchestration)).wait(STEP)?
        else {
            return None;
        };
        let TopicValue::Orchestration(view) = *value else {
            return None;
        };
        let root = view
            .roots
            .iter()
            .find(|root| root.target.as_ref() == Some(&opener))?;
        if root.children.len() != 2 {
            return None;
        }
        let group = root.children[0].fan_out.clone()?;
        for (index, target, cwd) in &children {
            let node = root
                .children
                .iter()
                .find(|node| node.target.as_ref() == Some(target))?;
            assert_eq!(node.handle.as_deref(), Some(handles[*index].as_str()));
            assert_eq!(node.fan_out.as_deref(), Some(group.as_str()));
            assert_eq!(node.cwd, *cwd);
            assert_eq!(
                node.worktree.as_ref()?.branch.as_deref(),
                Some(FAN_BRANCHES[*index])
            );
        }
        Some(group)
    });
    let parent_file = session_file(&gui, &opener);
    std::fs::write(markers.join("children-registered"), "continue").expect("release initial turns");
    let combined_title = format!("Fan-out {fan_out} finished (2/2 done)");
    let delivered = wait_for(
        "one combined result really reaches the parent model",
        TURN,
        || {
            model.find(0, |req| {
                fm::has_tools(req)
                    && conversation_text(req).contains("FAN-PARENT-OPEN")
                    && fm::messages(req).iter().any(|message| {
                        fm::role(message) == "user" && fm::text(message).contains(&combined_title)
                    })
            })
        },
    );
    for index in 0..2 {
        let text = conversation_text(&delivered);
        for expected in [
            handles[index].as_str(),
            FAN_TITLES[index],
            FAN_BRANCHES[index],
            FAN_REPLIES[index],
        ] {
            assert!(
                text.contains(expected),
                "combined result missing {expected}: {text}"
            );
        }
        idle(&gui, &children[index].1);
        assert_eq!(
            fixture_git(&children[index].2, &["status", "--porcelain"]),
            ""
        );
        assert!(
            !primary
                .join(format!("fan-{}.txt", if index == 0 { "a" } else { "b" }))
                .exists()
        );
    }
    let child_tips: Vec<_> = children
        .iter()
        .map(|(_, _, cwd)| fixture_git(cwd, &["rev-parse", "HEAD"]))
        .collect();
    prompt(&gui, &opener, "FAN-PARENT-FINISH");
    wait_for("first finish actually owns the merge lane", TURN, || {
        markers.join("first-entered").is_file().then_some(())
    });
    wait_for(
        "second agent finish visibly queues before first release",
        TURN,
        || {
            let Ok(Answer::Topic(value)) = gui
                .query(Query::Topic(Topic::Worktrees(primary.clone())))
                .wait(STEP)?
            else {
                return None;
            };
            let TopicValue::Worktrees(view) = *value else {
                return None;
            };
            let first = view
                .linked
                .iter()
                .find(|entry| entry.worktree.path == children[0].2)?;
            let second = view
                .linked
                .iter()
                .find(|entry| entry.worktree.path == children[1].2)?;
            (matches!(
                first.flow,
                Some(WorktreeFlow::Finishing {
                    state: OrchFinish::Merging,
                    ..
                })
            ) && matches!(
                second.flow,
                Some(WorktreeFlow::Finishing {
                    state: OrchFinish::Queued { ahead: 1.. },
                    ..
                })
            ))
            .then_some(())
        },
    );
    assert_eq!(fixture_git(&primary, &["rev-parse", "HEAD"]), initial_tip);
    assert!(
        !markers.join("second-entered").exists(),
        "queued child must do nothing"
    );
    assert!(
        model
            .find(0, |req| {
                fm::last_user_text(req).contains("Finish this worktree:")
                    && fm::last_user_text(req).contains(FAN_BRANCHES[1])
            })
            .is_none(),
        "queued finish must not inject its prompt before lane release"
    );
    assert_eq!(
        std::fs::read_to_string(markers.join("merge-events")).expect("held lane events"),
        "a-start\n"
    );
    std::fs::write(markers.join("release-first"), "continue").expect("release first merge lane");
    wait_for(
        "both actual finish removal asides reach the parent",
        Duration::from_secs(180),
        || {
            let entries = session_entries(&parent_file);
            FAN_TITLES
                .iter()
                .zip(&handles)
                .all(|(title, handle)| {
                    entries.iter().any(|entry| {
                        entry["type"] == "custom_message"
                            && entry["customType"] == "ompdeck-wait"
                            && entry["content"].as_str().is_some_and(|text| {
                                text.contains(&format!("Worktree finish: {title}"))
                                    && text.contains(handle)
                                    && text.to_lowercase().contains("removed")
                            })
                    })
                })
                .then_some(())
        },
    );
    assert_eq!(
        std::fs::read_to_string(markers.join("merge-events")).expect("actual merge order"),
        "a-start\na-end\nb-start\nb-end\n"
    );
    assert!(!markers.join("merge-lock").exists());
    for (index, _, cwd) in &children {
        assert!(!cwd.exists(), "finished worktree removed");
        assert!(
            !fixture_git(&primary, &["worktree", "list", "--porcelain"])
                .contains(&*cwd.to_string_lossy()),
            "finished worktree unregistered"
        );
        assert_eq!(
            fixture_git(&primary, &["branch", "--list", FAN_BRANCHES[*index]]),
            ""
        );
        fixture_git(
            &primary,
            &["merge-base", "--is-ancestor", &child_tips[*index], "HEAD"],
        );
        let lane = if *index == 0 { "a" } else { "b" };
        assert_eq!(
            std::fs::read_to_string(primary.join(format!("fan-{lane}.txt")))
                .expect("real merged file"),
            format!("isolated {lane}\n")
        );
    }
    assert_eq!(
        fixture_git(
            &primary,
            &[
                "log",
                "--first-parent",
                "--format=%s",
                &format!("{initial_tip}..HEAD")
            ]
        ),
        "merge fan b\nmerge fan a"
    );
    assert_eq!(fixture_git(&primary, &["status", "--porcelain"]), "");
    idle(&gui, &opener);
    let entries = session_entries(&parent_file);
    let combined: Vec<_> = entries
        .iter()
        .filter(|entry| {
            entry["type"] == "custom_message"
                && entry["customType"] == "ompdeck-wait"
                && entry["details"]["title"]
                    .as_str()
                    .is_some_and(|title| title.starts_with(&format!("Fan-out {fan_out} finished")))
        })
        .collect();
    assert_eq!(
        combined.len(),
        1,
        "one persisted Deliver, not one per child: {combined:?}"
    );
    assert_eq!(combined[0]["details"]["title"], combined_title);
    for expected in FAN_REPLIES {
        assert!(
            combined[0]["content"]
                .as_str()
                .expect("combined content")
                .contains(expected)
        );
    }

    // A read-only initial turn must not snapshot. The next turn's bash verifies
    // the snapshot exists and contains the old bytes before it performs the edit.
    std::fs::write(primary.join("checkpoint-file.txt"), "before checkpoint\n")
        .expect("checkpoint fixture");
    std::fs::write(primary.join("index-only.txt"), "staged fixture\n").expect("staged fixture");
    fixture_git(&primary, &["add", "index-only.txt"]);
    std::fs::write(primary.join("index-only.txt"), "unstaged fixture\n").expect("unstaged fixture");
    let index_path = PathBuf::from(fixture_git(
        &primary,
        &["rev-parse", "--path-format=absolute", "--git-path", "index"],
    ));
    let index_before = std::fs::read(&index_path).expect("user index before capture");
    let editor = launch(&gui, &sb, "checkpoint-editor", "CHECKPOINT-READ");
    result(&model, "CHECKPOINT-READ");
    idle(&gui, &editor);
    let editor_file = session_file(&gui, &editor);
    gui.subscribe(Topic::Checkpoints(editor_file.clone()));
    let view = wait_for(
        "subscribed checkpoint identity after read-only turn",
        TURN,
        || {
            let Ok(Answer::Topic(value)) = gui
                .query(Query::Topic(Topic::Checkpoints(editor_file.clone())))
                .wait(STEP)?
            else {
                return None;
            };
            let TopicValue::Checkpoints(view) = *value else {
                return None;
            };
            (view.repo.as_ref() == Some(&primary) && view.session_id.is_some()).then_some(view)
        },
    );
    assert!(view.error.is_none(), "{view:?}");
    assert!(view.items.is_empty(), "read must not create a checkpoint");
    let session_id = view.session_id.expect("checkpoint session id");
    let ref_prefix = format!("refs/ompdeck/checkpoints/{session_id}/");
    assert_eq!(
        fixture_git(
            &primary,
            &["for-each-ref", "--format=%(refname)", &ref_prefix]
        ),
        ""
    );
    prompt(
        &gui,
        &editor,
        &format!("CHECKPOINT-EDIT session={session_id}"),
    );
    let edited = result(&model, "CHECKPOINT-EDIT");
    assert!(
        edited.contains("CHECKPOINT-CAPTURED-BEFORE-EDIT"),
        "{edited}"
    );
    idle(&gui, &editor);
    assert_eq!(
        std::fs::read_to_string(primary.join("checkpoint-file.txt")).expect("actual agent edit"),
        "after checkpoint\n"
    );
    let checkpoint = wait_for("automatic pre-edit checkpoint publication", TURN, || {
        let Ok(Answer::Topic(value)) = gui
            .query(Query::Topic(Topic::Checkpoints(editor_file.clone())))
            .wait(STEP)?
        else {
            return None;
        };
        let TopicValue::Checkpoints(view) = *value else {
            return None;
        };
        assert!(view.error.is_none(), "{view:?}");
        view.items.into_iter().find(|item| {
            item.kind == deck_core::checkpoints::CheckpointKind::Turn
                && item.prompt.contains("CHECKPOINT-EDIT")
        })
    });
    assert!(
        checkpoint.entry.is_some(),
        "checkpoint links to the persisted user entry"
    );
    assert_eq!(
        fixture_git(
            &primary,
            &[
                "show",
                &format!("{}:checkpoint-file.txt", checkpoint.commit)
            ]
        ),
        "before checkpoint"
    );
    assert_eq!(
        std::fs::read(&index_path).expect("index after capture"),
        index_before
    );
    let diff = gui
        .query(Query::CheckpointDiff {
            session: editor_file.clone(),
            seq: checkpoint.seq,
        })
        .wait(STEP)
        .expect("checkpoint diff response")
        .expect("checkpoint diff");
    let Answer::CheckpointDiff { rows: files, total } = diff else {
        panic!("checkpoint diff answer: {diff:?}");
    };
    assert_eq!(total, 1, "the full diff count must include the agent edit");
    assert_eq!(files.len(), 1, "only the agent edit differs: {files:?}");
    assert_eq!(files[0].path, "checkpoint-file.txt");
    assert_eq!(files[0].status, 'M');
    assert_eq!(files[0].added, Some(1));
    assert_eq!(files[0].removed, Some(1));
    let done = run(
        &gui,
        Mutation::CheckpointRestore {
            target: editor.clone(),
            session: editor_file.clone(),
            seq: checkpoint.seq,
        },
    )
    .expect("idle gateway checkpoint restore");
    let Done::CheckpointRestored {
        safety: Some(safety),
        changed,
    } = done
    else {
        panic!("restore must return its real safety checkpoint: {done:?}");
    };
    assert!(changed >= 1);
    assert_ne!(safety, checkpoint.seq);
    assert_eq!(
        std::fs::read_to_string(primary.join("checkpoint-file.txt")).expect("restored agent edit"),
        "before checkpoint\n"
    );
    assert_eq!(
        std::fs::read_to_string(primary.join("index-only.txt")).expect("preserved unstaged file"),
        "unstaged fixture\n"
    );
    assert_eq!(
        std::fs::read(&index_path).expect("index after restore"),
        index_before
    );
    let safety_ref = format!("{ref_prefix}{safety:06}");
    assert_eq!(
        fixture_git(
            &primary,
            &["show", &format!("{safety_ref}:checkpoint-file.txt")]
        ),
        "after checkpoint"
    );
    assert!(
        fixture_git(&primary, &["show", "-s", "--format=%B", &safety_ref])
            .lines()
            .any(|line| line == "kind: rollback"),
        "safety ref is a real rollback checkpoint"
    );
    wait_for(
        "safety checkpoint appears on subscribed topic",
        TURN,
        || {
            let Ok(Answer::Topic(value)) = gui
                .query(Query::Topic(Topic::Checkpoints(editor_file.clone())))
                .wait(STEP)?
            else {
                return None;
            };
            let TopicValue::Checkpoints(view) = *value else {
                return None;
            };
            view.items
                .iter()
                .any(|item| {
                    item.seq == safety
                        && item.kind == deck_core::checkpoints::CheckpointKind::Rollback
                })
                .then_some(())
        },
    );
    gui.unsubscribe(&Topic::Checkpoints(editor_file));
    run(&gui, Mutation::Kill(editor)).expect("stop checkpoint editor");
    for (_, child, _) in children {
        // Retirement may replace the controller generation while preserving its key.
        let target = wait_for("relocated fan-out child controller", STEP, || {
            let Ok(Answer::Topic(value)) =
                gui.query(Query::Topic(Topic::Controllers)).wait(STEP)?
            else {
                return None;
            };
            let TopicValue::Controllers(controllers) = *value else {
                return None;
            };
            controllers
                .into_iter()
                .find(|info| info.target.key == child.key)
                .map(|info| info.target)
        });
        run(&gui, Mutation::Kill(target)).expect("stop relocated fan-out child");
    }
    run(&gui, Mutation::Kill(opener)).expect("stop fan-out parent");
}

#[test]
fn real_agent_tools_notify_list_review_and_require_a_window() {
    // Sandbox owns process-wide HOME/XDG variables: keep all scenarios in one test.
    let _omp_override = test_omp_override();
    exercise_notify_list_review_and_require_a_window();
    exercise_open_wait_send_follow_up_draft_and_stale_handle();
    exercise_background_wait_idle_and_busy_delivery();
    exercise_forwarded_child_questions();
    exercise_worktree_report_progress_and_finish();
    exercise_fan_out_merge_queue_and_checkpoints();
}
