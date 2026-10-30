#![cfg(feature = "noise")]
//! The device application protocol against `tests/vectors/api-v1.json`, which the PWA tests read
//! too: record bytes both ways, and one canonical JSON text per message variant (decode equals
//! the Rust value, encode equals the text).
//!
//! Rewrite after an intentional protocol change:
//! `OMPDECK_WRITE_VECTORS=1 cargo test -p deck-remote --test api`

use std::collections::BTreeSet;

use deck_remote::api::*;
use serde_json::{Value, json};

const PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/vectors/api-v1.json");

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(text: &str) -> Vec<u8> {
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).expect("hex"))
        .collect()
}

fn live() -> LiveRef {
    LiveRef {
        key: "tmux:8:omp-deck:omp-1a2b".into(),
        generation: 7,
    }
}

const PROJECT: &str = "p-0123456789abcdef";
const SESSION: &str = "01HZX3J6V0Q8";

fn requests() -> Vec<(&'static str, Request)> {
    let r = |id: u64, seq: Option<u64>, op: Op| Request { id, seq, op };
    let files = || vec!["src/main.rs".to_owned(), "docs/".to_owned()];
    vec![
        (
            "subscribe projects",
            r(
                1,
                None,
                Op::Subscribe {
                    feed: Feed::Projects {},
                },
            ),
        ),
        (
            "subscribe sessions",
            r(
                41,
                None,
                Op::Subscribe {
                    feed: Feed::Sessions {
                        project: PROJECT.into(),
                        limit: 50,
                    },
                },
            ),
        ),
        (
            "subscribe live",
            r(
                2,
                None,
                Op::Subscribe {
                    feed: Feed::Live {},
                },
            ),
        ),
        (
            "subscribe inbox",
            r(
                3,
                None,
                Op::Subscribe {
                    feed: Feed::Inbox {},
                },
            ),
        ),
        (
            "subscribe cards",
            r(
                4,
                None,
                Op::Subscribe {
                    feed: Feed::Cards {
                        sessions: vec![SESSION.into()],
                    },
                },
            ),
        ),
        (
            "subscribe transcript of a subagent",
            r(
                5,
                None,
                Op::Subscribe {
                    feed: Feed::Transcript {
                        session: SESSION.into(),
                        agent: Some("CoreStatus".into()),
                        tail: 500,
                    },
                },
            ),
        ),
        (
            "unsubscribe changes",
            r(
                6,
                None,
                Op::Unsubscribe {
                    feed: Feed::Changes {
                        project: PROJECT.into(),
                    },
                },
            ),
        ),
        ("outcome", r(7, None, Op::Outcome { seq: 41 })),
        ("models", r(8, None, Op::Models {})),
        (
            "diff",
            r(
                9,
                None,
                Op::Diff {
                    project: PROJECT.into(),
                    file: "src/main.rs".into(),
                    side: DiffSide::Unstaged,
                },
            ),
        ),
        (
            "attach observe",
            r(
                10,
                None,
                Op::Attach {
                    live: live(),
                    mode: ViewMode::Observe,
                },
            ),
        ),
        (
            "attach control",
            r(
                11,
                None,
                Op::Attach {
                    live: live(),
                    mode: ViewMode::Control,
                },
            ),
        ),
        ("detach", r(12, None, Op::Detach { view: 3 })),
        ("ping", r(13, None, Op::Ping {})),
        (
            "new_session",
            r(
                14,
                Some(1),
                Op::NewSession {
                    project: PROJECT.into(),
                },
            ),
        ),
        (
            "resume",
            r(
                15,
                Some(2),
                Op::Resume {
                    session: SESSION.into(),
                },
            ),
        ),
        ("restart", r(16, Some(3), Op::Restart { live: live() })),
        ("stop", r(17, Some(4), Op::Stop { live: live() })),
        ("kill", r(18, Some(5), Op::Kill { live: live() })),
        (
            "prompt",
            r(
                19,
                Some(6),
                Op::Prompt {
                    live: live(),
                    text: "Explain \"main\"\nplease".into(),
                },
            ),
        ),
        ("interrupt", r(20, Some(7), Op::Interrupt { live: live() })),
        (
            "switch_model without level",
            r(
                21,
                Some(8),
                Op::SwitchModel {
                    live: live(),
                    selector: "anthropic/claude-sonnet-4".into(),
                    level: None,
                },
            ),
        ),
        (
            "set_effort",
            r(
                22,
                Some(9),
                Op::SetEffort {
                    live: live(),
                    level: "high".into(),
                },
            ),
        ),
        (
            "action handoff",
            r(
                23,
                Some(10),
                Op::Action {
                    live: live(),
                    action: SessionAction::Handoff { focus: None },
                },
            ),
        ),
        (
            "action compact focus",
            r(
                24,
                Some(11),
                Op::Action {
                    live: live(),
                    action: SessionAction::Compact {
                        mode: CompactMode::Focus,
                        focus: Some("the parser".into()),
                    },
                },
            ),
        ),
        (
            "action rename",
            r(
                25,
                Some(12),
                Op::Action {
                    live: live(),
                    action: SessionAction::Rename {
                        name: "Fix the parser ✓".into(),
                    },
                },
            ),
        ),
        (
            "action auto_name",
            r(
                26,
                Some(13),
                Op::Action {
                    live: live(),
                    action: SessionAction::AutoName {},
                },
            ),
        ),
        (
            "action plan",
            r(
                27,
                Some(14),
                Op::Action {
                    live: live(),
                    action: SessionAction::Plan {},
                },
            ),
        ),
        (
            "action retry",
            r(
                28,
                Some(15),
                Op::Action {
                    live: live(),
                    action: SessionAction::Retry {},
                },
            ),
        ),
        (
            "answer",
            r(
                29,
                Some(16),
                Op::Answer {
                    live: live(),
                    request: 12,
                    proceed: false,
                },
            ),
        ),
        ("cancel", r(30, Some(17), Op::Cancel { seq: 14 })),
        (
            "inbox_open",
            r(31, Some(18), Op::InboxOpen { live: live() }),
        ),
        (
            "inbox_dismiss",
            r(32, Some(19), Op::InboxDismiss { live: live() }),
        ),
        ("inbox_clear", r(33, Some(20), Op::InboxClear {})),
        (
            "stage",
            r(
                34,
                Some(21),
                Op::Stage {
                    project: PROJECT.into(),
                    files: files(),
                },
            ),
        ),
        (
            "unstage",
            r(
                35,
                Some(22),
                Op::Unstage {
                    project: PROJECT.into(),
                    files: files(),
                },
            ),
        ),
        (
            "delete_sessions",
            r(
                36,
                Some(23),
                Op::DeleteSessions {
                    sessions: vec![SESSION.into()],
                },
            ),
        ),
        (
            "copy_session",
            r(
                37,
                Some(24),
                Op::CopySession {
                    session: SESSION.into(),
                    project: PROJECT.into(),
                },
            ),
        ),
        (
            "move_session",
            r(
                38,
                Some(25),
                Op::MoveSession {
                    session: SESSION.into(),
                    project: PROJECT.into(),
                },
            ),
        ),
        (
            "discard",
            r(
                39,
                Some(26),
                Op::Discard {
                    project: PROJECT.into(),
                    files: files(),
                },
            ),
        ),
        (
            "delete_untracked",
            r(
                40,
                Some(27),
                Op::DeleteUntracked {
                    project: PROJECT.into(),
                    files: files(),
                },
            ),
        ),
    ]
}

fn delivered() -> DeliveryView {
    DeliveryView::Delivered {}
}

fn replies() -> Vec<(&'static str, Reply)> {
    let ok = |id: u64, r: Resp| Reply {
        id,
        result: ReplyResult::Ok(r),
    };
    let err = |id: u64, e: ApiError| Reply {
        id,
        result: ReplyResult::Err(e),
    };
    let outcome = |id, o: OutcomeView| ok(id, Resp::Outcome { outcome: o });
    let done = |id, d: DoneView| outcome(id, OutcomeView::Done { result: d });
    let rejected = |id, r: RejectedView| outcome(id, OutcomeView::Rejected { reason: r });
    vec![
        ("unit", ok(1, Resp::Unit {})),
        ("pong", ok(2, Resp::Pong {})),
        (
            "accepted duplicate",
            ok(
                3,
                Resp::Accepted {
                    seq: 9,
                    duplicate: true,
                },
            ),
        ),
        ("outcome running", outcome(4, OutcomeView::Running {})),
        ("outcome done unit", done(5, DoneView::Unit {})),
        (
            "outcome done session",
            done(6, DoneView::Session { live: live() }),
        ),
        (
            "outcome done submitted",
            done(
                7,
                DoneView::Submitted {
                    request: 12,
                    label: "Rename".into(),
                    delivery: delivered(),
                },
            ),
        ),
        (
            "outcome done answered",
            done(8, DoneView::Answered { applied: true }),
        ),
        (
            "outcome done deleted",
            done(
                9,
                DoneView::Deleted {
                    results: vec![
                        SessionResult {
                            session: SESSION.into(),
                            error: None,
                        },
                        SessionResult {
                            session: "other".into(),
                            error: Some("permission denied".into()),
                        },
                    ],
                },
            ),
        ),
        (
            "outcome done file",
            done(
                10,
                DoneView::File {
                    session: Some(SESSION.into()),
                },
            ),
        ),
        (
            "outcome failed",
            outcome(
                11,
                OutcomeView::Failed {
                    message: "git failed".into(),
                },
            ),
        ),
        (
            "outcome rejected stale",
            rejected(
                12,
                RejectedView::Stale {
                    current: Some(live()),
                },
            ),
        ),
        (
            "outcome rejected exited",
            rejected(13, RejectedView::Exited {}),
        ),
        (
            "outcome rejected read_only",
            rejected(14, RejectedView::ReadOnly {}),
        ),
        (
            "outcome rejected detached",
            rejected(15, RejectedView::Detached {}),
        ),
        ("outcome rejected busy", rejected(16, RejectedView::Busy {})),
        (
            "outcome rejected no_bracketed_paste",
            rejected(17, RejectedView::NoBracketedPaste {}),
        ),
        (
            "outcome rejected not_injected",
            rejected(18, RejectedView::NotInjected {}),
        ),
        (
            "outcome rejected empty",
            rejected(19, RejectedView::Empty {}),
        ),
        (
            "outcome rejected no_change",
            rejected(20, RejectedView::NoChange {}),
        ),
        (
            "outcome rejected model_unknown",
            rejected(21, RejectedView::ModelUnknown {}),
        ),
        (
            "outcome rejected key_reused",
            rejected(22, RejectedView::KeyReused {}),
        ),
        (
            "outcome rejected expired",
            rejected(23, RejectedView::Expired {}),
        ),
        (
            "outcome rejected input_held",
            rejected(24, RejectedView::InputHeld {}),
        ),
        (
            "outcome rejected bad_path",
            rejected(25, RejectedView::BadPath {}),
        ),
        ("outcome cancelled", outcome(26, OutcomeView::Cancelled {})),
        ("outcome uncertain", outcome(27, OutcomeView::Uncertain {})),
        (
            "outcome recorded",
            outcome(28, OutcomeView::Recorded { succeeded: false }),
        ),
        ("outcome unknown", outcome(29, OutcomeView::Unknown {})),
        ("outcome expired", outcome(30, OutcomeView::Expired {})),
        (
            "models",
            ok(
                31,
                Resp::Models {
                    models: vec![Model {
                        selector: "anthropic/claude-sonnet-4".into(),
                        provider: "anthropic".into(),
                        id: "claude-sonnet-4".into(),
                        name: "Claude Sonnet 4".into(),
                        context_window: Some(200_000),
                        max_tokens: Some(64_000),
                        reasoning: Some(true),
                        thinking: vec!["low".into(), "high".into()],
                        input: vec!["text".into(), "image".into()],
                        cost_input: Some(3.0),
                        cost_output: Some(15.5),
                    }],
                    truncated: false,
                },
            ),
        ),
        (
            "diff",
            ok(
                32,
                Resp::Diff {
                    diff: Diff {
                        text: "@@ -1 +1 @@\n-a\n+b\n".into(),
                        binary: false,
                        truncated: false,
                        added: 1,
                        removed: 1,
                    },
                },
            ),
        ),
        (
            "attached",
            ok(
                33,
                Resp::Attached {
                    view: 2,
                    mode: ViewMode::Input,
                },
            ),
        ),
        ("error unsupported", err(34, ApiError::Unsupported {})),
        (
            "error not_allowed",
            err(
                35,
                ApiError::NotAllowed {
                    need: Class::Manage,
                },
            ),
        ),
        (
            "error bad_request",
            err(
                36,
                ApiError::BadRequest {
                    message: "unknown field `x`".into(),
                },
            ),
        ),
        (
            "error not_found",
            err(
                37,
                ApiError::NotFound {
                    what: "session".into(),
                },
            ),
        ),
        ("error stale", err(38, ApiError::Stale { current: None })),
        (
            "error rejected",
            err(
                39,
                ApiError::Rejected {
                    reason: RejectedView::Busy {},
                },
            ),
        ),
        ("error geometry_owned", err(40, ApiError::GeometryOwned {})),
        (
            "error too_many_requests",
            err(41, ApiError::TooManyRequests {}),
        ),
        ("error too_large", err(42, ApiError::TooLarge {})),
        ("error maintenance", err(43, ApiError::Maintenance {})),
        ("error key_reused", err(44, ApiError::KeyReused {})),
        ("error expired", err(45, ApiError::Expired {})),
        (
            "error failed",
            err(
                46,
                ApiError::Failed {
                    message: "no omp".into(),
                },
            ),
        ),
    ]
}

fn feeds() -> Vec<(&'static str, Event)> {
    let feed = |feed: Feed, value: FeedValue| Event::Feed { feed, value };
    let own = |delivery: DeliveryView| OwnRequest {
        request: 12,
        seq: 6,
        label: "Send prompt".into(),
        delivery,
    };
    let session = |status: Status, requests| LiveSession {
        live: live(),
        kind: LiveKind::Session,
        label: "Fix the parser".into(),
        title: Some("Fix the parser".into()),
        session: Some(SESSION.into()),
        project: Some(PROJECT.into()),
        display: "~/src/app".into(),
        activity: Activity::Busy,
        status,
        exit_code: None,
        busy: true,
        model: Some("anthropic/claude-sonnet-4".into()),
        pending_switch: Some(SwitchView {
            selector: Some("openai/gpt-5".into()),
            level: None,
        }),
        live_control: true,
        desktop_controlled: false,
        views: 1,
        requests,
    };
    let item = |kind: InboxKind| InboxItem {
        live: live(),
        kind,
        title: "Fix the parser".into(),
        project: "app".into(),
        body: "Finished its turn".into(),
        at_ms: 1_761_000_000_000,
        armed: true,
    };
    vec![
        (
            "feed projects",
            feed(
                Feed::Projects {},
                FeedValue::Projects {
                    projects: vec![Project {
                        id: PROJECT.into(),
                        display: "~/src/app".into(),
                        worktree: false,
                        session_count: 12,
                        modified_unix_ms: 1_761_000_000_000,
                    }],
                    truncated: false,
                },
            ),
        ),
        (
            "feed sessions",
            feed(
                Feed::Sessions {
                    project: PROJECT.into(),
                    limit: 1,
                },
                FeedValue::Sessions {
                    project: PROJECT.into(),
                    total: 12,
                    sessions: vec![SessionSummary {
                        id: SESSION.into(),
                        title: None,
                        first_prompt: Some("Explain main".into()),
                        modified_ms: 1_761_000_000_000,
                    }],
                    truncated: false,
                },
            ),
        ),
        (
            "feed live",
            feed(
                Feed::Live {},
                FeedValue::Live {
                    sessions: vec![
                        session(
                            Status::Busy {},
                            vec![
                                own(DeliveryView::Queued {}),
                                own(DeliveryView::AwaitingConfirm {
                                    draft: "half typed".into(),
                                }),
                                own(DeliveryView::Writing {}),
                                own(delivered()),
                                own(DeliveryView::Refused {
                                    reason: Refusal::Precondition,
                                }),
                                own(DeliveryView::Refused {
                                    reason: Refusal::DialogOpen,
                                }),
                                own(DeliveryView::Refused {
                                    reason: Refusal::NoBracketedPaste,
                                }),
                                own(DeliveryView::Declined {}),
                                own(DeliveryView::Aborted {}),
                                own(DeliveryView::Stalled {}),
                                own(DeliveryView::Cancelled {
                                    reason: CancelReason::SessionChanged,
                                }),
                                own(DeliveryView::Unobserved {}),
                            ],
                        ),
                        session(Status::Idle {}, vec![]),
                        session(Status::Attention {}, vec![]),
                        session(Status::Exited { clean: true }, vec![]),
                    ],
                    truncated: true,
                },
            ),
        ),
        (
            "feed inbox",
            feed(
                Feed::Inbox {},
                FeedValue::Inbox {
                    inbox: Inbox {
                        entries: vec![
                            item(InboxKind::Finished {}),
                            item(InboxKind::Error {
                                message: Some("overloaded".into()),
                            }),
                            item(InboxKind::Attention {}),
                            item(InboxKind::Exited { code: Some(1) }),
                        ],
                        warns: true,
                        badge: Some("4".into()),
                    },
                    truncated: false,
                },
            ),
        ),
        (
            "feed cards",
            feed(
                Feed::Cards {
                    sessions: vec![SESSION.into()],
                },
                FeedValue::Cards {
                    cards: vec![Card {
                        session: SESSION.into(),
                        model: Some("anthropic/claude-sonnet-4".into()),
                        thinking: Some("high".into()),
                        context_tokens: Some(42_000),
                        input_tokens: 1200,
                        output_tokens: 340,
                        cost: 0.25,
                        last_reply: Some("Done.".into()),
                        last_turn_failed: false,
                        git: Some(GitCounts {
                            branch: "main".into(),
                            ahead: 1,
                            behind: 0,
                            staged: 2,
                            unstaged: 3,
                            untracked: 4,
                        }),
                        agents: AgentCounts {
                            running: 1,
                            done: 2,
                            failed: 0,
                            total: 3,
                        },
                    }],
                    truncated: false,
                },
            ),
        ),
        (
            "feed transcript",
            feed(
                Feed::Transcript {
                    session: SESSION.into(),
                    agent: None,
                    tail: 50,
                },
                FeedValue::Transcript {
                    start: 0,
                    reset: true,
                    items: vec![
                        Item::User {
                            text: "hi".into(),
                            from_agent: false,
                            at_ms: Some(1_761_000_000_000),
                        },
                        Item::Assistant {
                            text: "hello".into(),
                            model: Some("anthropic/claude-sonnet-4".into()),
                            at_ms: None,
                        },
                        Item::Thinking { text: "hm".into() },
                        Item::ToolCall {
                            call_id: "c1".into(),
                            name: "bash".into(),
                            intent: Some("Running tests".into()),
                            summary: "cargo test".into(),
                            args: "{\"command\":\"cargo test\"}".into(),
                        },
                        Item::ToolResult {
                            call_id: "c1".into(),
                            name: "bash".into(),
                            text: "ok".into(),
                            is_error: false,
                            truncated: true,
                        },
                        Item::Notice {
                            kind: "async-result".into(),
                            text: "done".into(),
                        },
                        Item::Irc {
                            from: Some("parent".into()),
                            agent: Some("Main".into()),
                            text: "go".into(),
                            note: None,
                        },
                        Item::Compaction {
                            tokens_before: Some(100),
                            tokens_after: None,
                            summary: "short".into(),
                        },
                        Item::ModelChange {
                            model: "openai/gpt-5".into(),
                        },
                        Item::Error {
                            message: "overloaded".into(),
                        },
                        Item::Exit {
                            reason: "user".into(),
                        },
                    ],
                },
            ),
        ),
        (
            "feed changes",
            feed(
                Feed::Changes {
                    project: PROJECT.into(),
                },
                FeedValue::Changes {
                    changes: Some(Changes {
                        branch: Some("main".into()),
                        files: vec![
                            ChangedFile {
                                path: "src/new.rs".into(),
                                orig_path: Some("src/old.rs".into()),
                                staged: Some(ChangeKind::Renamed),
                                unstaged: Some(ChangeKind::Modified),
                                untracked: false,
                                conflicted: false,
                                is_dir: false,
                            },
                            ChangedFile {
                                path: "a".into(),
                                orig_path: None,
                                staged: Some(ChangeKind::Added),
                                unstaged: Some(ChangeKind::Deleted),
                                untracked: false,
                                conflicted: true,
                                is_dir: false,
                            },
                            ChangedFile {
                                path: "b".into(),
                                orig_path: None,
                                staged: Some(ChangeKind::Copied),
                                unstaged: Some(ChangeKind::TypeChanged),
                                untracked: false,
                                conflicted: false,
                                is_dir: false,
                            },
                            ChangedFile {
                                path: "docs/".into(),
                                orig_path: None,
                                staged: None,
                                unstaged: None,
                                untracked: true,
                                conflicted: false,
                                is_dir: true,
                            },
                        ],
                    }),
                    error: None,
                    truncated: false,
                },
            ),
        ),
    ]
}

fn events() -> Vec<(&'static str, Event)> {
    let mut out = feeds();
    out.extend([
        (
            "outcome",
            Event::Outcome {
                seq: 6,
                outcome: OutcomeView::Done {
                    result: DoneView::Unit {},
                },
            },
        ),
        (
            "progress",
            Event::Progress {
                seq: 23,
                stage: "Deleting".into(),
                done: 1,
                total: Some(3),
            },
        ),
        (
            "confirm",
            Event::Confirm {
                live: live(),
                request: 12,
                label: "Send prompt".into(),
                draft: "half typed".into(),
            },
        ),
        (
            "term_state",
            Event::TermState {
                view: 2,
                input_held: true,
            },
        ),
        (
            "input_refused",
            Event::InputRefused {
                view: 2,
                reason: InputRefusal::InputHeld,
            },
        ),
        (
            "model_unconfirmed",
            Event::ModelUnconfirmed {
                live: live(),
                selector: Some("openai/gpt-5".into()),
                level: None,
            },
        ),
        ("resync", Event::Resync {}),
    ]);
    out
}

fn welcomes() -> Vec<(&'static str, Welcome)> {
    vec![(
        "welcome",
        Welcome {
            protocol: PROTOCOL,
            gateway_version: "0.9.0".into(),
            epoch: "00112233445566778899aabbccddeeff".into(),
            device: "d-0123456789abcdef".into(),
            name: "Pixel".into(),
            permissions: Permissions {
                control: true,
                manage: false,
            },
            next_seq: 28,
            limits: Limits::default(),
            features: vec![],
        },
    )]
}

fn term_ends() -> Vec<(&'static str, TermEnd)> {
    vec![
        ("exited", TermEnd::Exited { code: Some(0) }),
        ("closed", TermEnd::Closed {}),
        ("stale", TermEnd::Stale {}),
        ("overflow", TermEnd::Overflow {}),
        ("disconnected", TermEnd::Disconnected {}),
        ("shutdown", TermEnd::Shutdown {}),
        ("detached", TermEnd::Detached {}),
        ("not_allowed", TermEnd::NotAllowed {}),
        ("too_large", TermEnd::TooLarge {}),
    ]
}

fn json_text<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_string(value).unwrap()
}

fn messages() -> Vec<Value> {
    let mut out = Vec::new();
    let mut push = |kind: &str, case: &str, text: String| {
        out.push(json!({"type": kind, "case": case, "json": text}));
    };
    for (case, v) in requests() {
        push("request", case, json_text(&v));
    }
    for (case, v) in replies() {
        push("reply", case, json_text(&v));
    }
    for (case, v) in events() {
        push("event", case, json_text(&v));
    }
    for (case, v) in welcomes() {
        push("welcome", case, json_text(&v));
    }
    for (case, v) in term_ends() {
        push("term_end", case, json_text(&v));
    }
    out
}

/// The vector form of a record.
fn record_json(r: &Record) -> Value {
    let text = |b: &[u8]| String::from_utf8(b.to_vec()).unwrap();
    match r {
        Record::Request(b) => json!({"kind": "request", "json": text(b)}),
        Record::Ack { messages } => json!({"kind": "ack", "messages": messages}),
        Record::Input {
            view,
            ctrl_c,
            bytes,
        } => json!({"kind": "input", "view": view, "ctrl_c": ctrl_c, "hex": hex(bytes)}),
        Record::Resize { view, cols, rows } => {
            json!({"kind": "resize", "view": view, "cols": cols, "rows": rows})
        }
        Record::Welcome(b) => json!({"kind": "welcome", "json": text(b)}),
        Record::Reply(b) => json!({"kind": "reply", "json": text(b)}),
        Record::Event(b) => json!({"kind": "event", "json": text(b)}),
        Record::Snapshot { view, last, bytes } => {
            json!({"kind": "t_snapshot", "view": view, "last": last, "hex": hex(bytes)})
        }
        Record::Output { view, bytes } => {
            json!({"kind": "t_output", "view": view, "hex": hex(bytes)})
        }
        Record::TermResize { view, cols, rows } => {
            json!({"kind": "t_resize", "view": view, "cols": cols, "rows": rows})
        }
        Record::TermEnd { view, end } => {
            json!({"kind": "t_end", "view": view, "json": text(end)})
        }
    }
}

fn accepted_records() -> Vec<(&'static str, Origin, Vec<Record>)> {
    let req = |r: &Request| Record::Request(serde_json::to_vec(r).unwrap());
    let reqs = requests();
    let reply = Record::Reply(serde_json::to_vec(&replies()[0].1).unwrap());
    let event = Record::Event(serde_json::to_vec(&Event::Resync {}).unwrap());
    let welcome = Record::Welcome(serde_json::to_vec(&welcomes()[0].1).unwrap());
    vec![
        ("one request", Origin::Device, vec![req(&reqs[12].1)]),
        (
            "requests, ack, input and resize batched",
            Origin::Device,
            vec![
                req(&reqs[0].1),
                Record::Ack { messages: 300 },
                Record::Input {
                    view: 2,
                    ctrl_c: false,
                    bytes: b"ls\r".to_vec(),
                },
                Record::Input {
                    view: 2,
                    ctrl_c: true,
                    bytes: vec![3],
                },
                Record::Resize {
                    view: 2,
                    cols: 120,
                    rows: 40,
                },
                req(&reqs[24].1),
            ],
        ),
        (
            "empty input",
            Origin::Device,
            vec![Record::Input {
                view: 1,
                ctrl_c: false,
                bytes: vec![],
            }],
        ),
        (
            "welcome then a reply and an event",
            Origin::Gateway,
            vec![welcome, reply, event],
        ),
        (
            "terminal records",
            Origin::Gateway,
            vec![
                Record::TermResize {
                    view: 2,
                    cols: 120,
                    rows: 40,
                },
                Record::Snapshot {
                    view: 2,
                    last: false,
                    bytes: b"\x1b[2J".to_vec(),
                },
                Record::Snapshot {
                    view: 2,
                    last: true,
                    bytes: b"$ ".to_vec(),
                },
                Record::Output {
                    view: 2,
                    bytes: "héllo".as_bytes().to_vec(),
                },
                Record::TermEnd {
                    view: 2,
                    end: serde_json::to_vec(&TermEnd::Overflow {}).unwrap(),
                },
            ],
        ),
    ]
}

/// `(case, origin, hex, error)`: messages a receiver refuses whole.
fn rejected_records() -> Vec<(&'static str, Origin, String, RecordError)> {
    let mut big = vec![kind::T_SNAPSHOT];
    let len = 5 + MAX_SNAPSHOT_CHUNK + 1;
    big.extend_from_slice(&(len as u32).to_be_bytes());
    big.extend_from_slice(&[0, 0, 0, 1, 1]);
    big.extend(std::iter::repeat_n(0u8, MAX_SNAPSHOT_CHUNK + 1));
    let mut input = vec![kind::INPUT];
    let len = 5 + MAX_INPUT + 1;
    input.extend_from_slice(&(len as u32).to_be_bytes());
    input.extend_from_slice(&[0, 0, 0, 1, 0]);
    input.extend(std::iter::repeat_n(b'a', MAX_INPUT + 1));
    vec![
        (
            "empty message",
            Origin::Device,
            String::new(),
            RecordError::Empty,
        ),
        (
            "empty message from a gateway",
            Origin::Gateway,
            String::new(),
            RecordError::Empty,
        ),
        (
            "short header",
            Origin::Device,
            "01000000".into(),
            RecordError::Truncated,
        ),
        (
            "body past the end",
            Origin::Device,
            "0100000005".to_owned() + "7b7d",
            RecordError::Truncated,
        ),
        (
            "unknown kind from a device",
            Origin::Device,
            "7f00000000".into(),
            RecordError::UnknownKind(0x7f),
        ),
        (
            "gateway kind from a device",
            Origin::Device,
            "1000000002".to_owned() + "7b7d",
            RecordError::WrongDirection(kind::WELCOME),
        ),
        (
            "device kind from a gateway",
            Origin::Gateway,
            "0200000008".to_owned() + "0000000000000001",
            RecordError::WrongDirection(kind::ACK),
        ),
        (
            "ack of 7 bytes",
            Origin::Device,
            "0200000007".to_owned() + "00000000000001",
            RecordError::Malformed(kind::ACK),
        ),
        (
            "input with an unknown flag",
            Origin::Device,
            "0300000006".to_owned() + "000000010261",
            RecordError::Malformed(kind::INPUT),
        ),
        (
            "input without flags",
            Origin::Device,
            "0300000004".to_owned() + "00000001",
            RecordError::Malformed(kind::INPUT),
        ),
        (
            "input over 64 KiB",
            Origin::Device,
            hex(&input),
            RecordError::Oversized {
                kind: kind::INPUT,
                len: MAX_INPUT + 1,
                max: MAX_INPUT,
            },
        ),
        (
            "resize of 9 bytes",
            Origin::Device,
            "0400000009".to_owned() + "000000010050001800",
            RecordError::Malformed(kind::RESIZE),
        ),
        (
            "t_resize of 7 bytes",
            Origin::Gateway,
            "2200000007".to_owned() + "00000001005000",
            RecordError::Malformed(kind::T_RESIZE),
        ),
        (
            "t_snapshot with last = 2",
            Origin::Gateway,
            "2000000006".to_owned() + "000000010261",
            RecordError::Malformed(kind::T_SNAPSHOT),
        ),
        (
            "t_snapshot chunk over 256 KiB",
            Origin::Gateway,
            hex(&big),
            RecordError::Oversized {
                kind: kind::T_SNAPSHOT,
                len: MAX_SNAPSHOT_CHUNK + 1,
                max: MAX_SNAPSHOT_CHUNK,
            },
        ),
        (
            "t_output without a view",
            Origin::Gateway,
            "2100000003".to_owned() + "000001",
            RecordError::Malformed(kind::T_OUTPUT),
        ),
    ]
}

fn error_name(e: &RecordError) -> &'static str {
    match e {
        RecordError::Empty => "empty",
        RecordError::TooLarge(_) => "too_large",
        RecordError::Truncated => "truncated",
        RecordError::UnknownKind(_) => "unknown_kind",
        RecordError::WrongDirection(_) => "wrong_direction",
        RecordError::Malformed(_) => "malformed",
        RecordError::Oversized { .. } => "oversized",
    }
}

fn origin_name(o: Origin) -> &'static str {
    match o {
        Origin::Device => "device",
        Origin::Gateway => "gateway",
    }
}

/// `(case, json, expected)`: request bodies that are answered or close the session.
fn rejected_requests() -> Vec<(&'static str, &'static str, Value)> {
    vec![
        (
            "unknown op",
            r#"{"id":5,"op":{"op":"teleport"}}"#,
            json!({"error": "unsupported", "id": 5}),
        ),
        (
            "unknown field in an op",
            r#"{"id":6,"op":{"op":"ping","x":1}}"#,
            json!({"error": "bad_request", "id": 6}),
        ),
        (
            "unknown field in the request",
            r#"{"id":7,"op":{"op":"ping"},"x":1}"#,
            json!({"error": "bad_request", "id": 7}),
        ),
        (
            "unknown field in a live ref",
            r#"{"id":8,"seq":1,"op":{"op":"kill","live":{"key":"k","generation":1,"epoch":"x"}}}"#,
            json!({"error": "bad_request", "id": 8}),
        ),
        (
            "unknown feed",
            r#"{"id":9,"op":{"op":"subscribe","feed":{"feed":"weather"}}}"#,
            json!({"error": "bad_request", "id": 9}),
        ),
        (
            "missing required field",
            r#"{"id":10,"seq":1,"op":{"op":"prompt","live":{"key":"k","generation":1}}}"#,
            json!({"error": "bad_request", "id": 10}),
        ),
        (
            "wrong type",
            r#"{"id":11,"op":{"op":"outcome","seq":"1"}}"#,
            json!({"error": "bad_request", "id": 11}),
        ),
        (
            "no id",
            r#"{"op":{"op":"ping"}}"#,
            json!({"error": "unreadable"}),
        ),
        (
            "string id",
            r#"{"id":"1","op":{"op":"ping"}}"#,
            json!({"error": "unreadable"}),
        ),
        ("not json", "ping", json!({"error": "unreadable"})),
        ("not an object", "[1]", json!({"error": "unreadable"})),
    ]
}

fn build() -> Value {
    let records: Vec<Value> = accepted_records()
        .into_iter()
        .map(|(case, origin, records)| {
            json!({
                "case": case,
                "origin": origin_name(origin),
                "hex": hex(&encode(&records).unwrap()),
                "records": records.iter().map(record_json).collect::<Vec<_>>(),
            })
        })
        .collect();
    // Long messages are a head plus a run of one byte: `hex` + `pad { byte, count }`.
    let rejected: Vec<Value> = rejected_records()
        .into_iter()
        .map(|(case, origin, text, e)| {
            let bytes = unhex(&text);
            let mut v = json!({"case": case, "origin": origin_name(origin), "hex": text, "error": error_name(&e)});
            if bytes.len() > 64 {
                let (head, run) = bytes.split_at(10);
                assert!(run.iter().all(|b| *b == run[0]), "{case}");
                v["hex"] = json!(hex(head));
                v["pad"] = json!({"byte": run[0], "count": run.len()});
            }
            v
        })
        .collect();
    let requests: Vec<Value> = rejected_requests()
        .into_iter()
        .map(|(case, text, expect)| {
            let mut v = json!({"case": case, "json": text});
            if let (Some(o), Some(e)) = (v.as_object_mut(), expect.as_object()) {
                o.extend(e.clone());
            }
            v
        })
        .collect();
    json!({
        "description": "Device application protocol v1 (deck_remote::api) shared with pwa/src/remote. records: whole application messages (hex) and their records; rejected_records: messages refused whole; messages: the canonical JSON text of every message variant (decode equals the value, encode equals the text; None is null); rejected_requests: REQUEST bodies answered unsupported/bad_request (by id) or closing the session (unreadable). Generated by crates/deck-remote/tests/api.rs.",
        "protocol": PROTOCOL,
        "records": records,
        "rejected_records": rejected,
        "messages": messages(),
        "rejected_requests": requests,
    })
}

#[test]
fn shared_vectors_match() {
    let built = build();
    let text = serde_json::to_string_pretty(&built).unwrap() + "\n";
    if std::env::var_os("OMPDECK_WRITE_VECTORS").is_some() {
        std::fs::write(PATH, &text).unwrap();
    }
    let file = std::fs::read_to_string(PATH).expect("api-v1.json (OMPDECK_WRITE_VECTORS=1)");
    let on_disk: Value = serde_json::from_str(&file).unwrap();
    assert!(
        on_disk == built,
        "api-v1.json drifted: OMPDECK_WRITE_VECTORS=1"
    );
}

#[test]
fn records_decode_and_encode_both_ways() {
    let file: Value = serde_json::from_str(&std::fs::read_to_string(PATH).unwrap()).unwrap();
    let expected = accepted_records();
    let vectors = file["records"].as_array().unwrap();
    assert_eq!(vectors.len(), expected.len());
    for (v, (case, origin, records)) in vectors.iter().zip(expected) {
        let bytes = unhex(v["hex"].as_str().unwrap());
        assert_eq!(decode(&bytes, origin).unwrap(), records, "{case}");
        assert_eq!(encode(&records).unwrap(), bytes, "{case}");
    }
    for v in file["rejected_records"].as_array().unwrap() {
        let origin = match v["origin"].as_str().unwrap() {
            "device" => Origin::Device,
            _ => Origin::Gateway,
        };
        let mut bytes = unhex(v["hex"].as_str().unwrap());
        if let Some(pad) = v.get("pad") {
            let byte = pad["byte"].as_u64().unwrap() as u8;
            bytes.extend(std::iter::repeat_n(
                byte,
                pad["count"].as_u64().unwrap() as usize,
            ));
        }
        let err = decode(&bytes, origin).unwrap_err();
        assert_eq!(
            error_name(&err),
            v["error"].as_str().unwrap(),
            "{}",
            v["case"]
        );
    }
}

#[test]
fn every_message_variant_decodes_to_its_value_and_encodes_to_its_text() {
    let file: Value = serde_json::from_str(&std::fs::read_to_string(PATH).unwrap()).unwrap();
    let mut by_type: std::collections::BTreeMap<&str, Vec<(&str, &str)>> = Default::default();
    for m in file["messages"].as_array().unwrap() {
        by_type
            .entry(m["type"].as_str().unwrap())
            .or_default()
            .push((m["case"].as_str().unwrap(), m["json"].as_str().unwrap()));
    }
    fn check<T>(vectors: &[(&str, &str)], values: Vec<(&str, T)>)
    where
        T: serde::Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug,
    {
        assert_eq!(vectors.len(), values.len());
        for ((case, text), (_, value)) in vectors.iter().zip(values) {
            let decoded: T = serde_json::from_str(text).unwrap_or_else(|e| panic!("{case}: {e}"));
            assert_eq!(decoded, value, "{case}");
            assert_eq!(serde_json::to_string(&value).unwrap(), *text, "{case}");
        }
    }
    check(&by_type["request"], requests());
    check(&by_type["reply"], replies());
    check(&by_type["event"], events());
    check(&by_type["welcome"], welcomes());
    check(&by_type["term_end"], term_ends());
    for (_, text) in &by_type["request"] {
        assert!(Request::parse(text.as_bytes()).is_ok(), "{text}");
    }
}

#[test]
fn rejected_requests_are_classified() {
    for (case, text, expect) in rejected_requests() {
        let got = Request::parse(text.as_bytes()).unwrap_err();
        let (name, id) = match got {
            RequestError::Unreadable => ("unreadable", None),
            RequestError::Unsupported { id } => ("unsupported", Some(id)),
            RequestError::Bad { id, .. } => ("bad_request", Some(id)),
        };
        assert_eq!(name, expect["error"], "{case}");
        assert_eq!(id, expect["id"].as_u64(), "{case}");
    }
}

/// Collects `(tag field, value)` pairs from every message.
fn tags(value: &Value, out: &mut BTreeSet<(String, String)>) {
    match value {
        Value::Object(map) => {
            for (k, v) in map {
                if let ("op" | "feed" | "type" | "state" | "code" | "reason", Value::String(s)) =
                    (k.as_str(), v)
                {
                    out.insert((k.clone(), s.clone()));
                }
                tags(v, out);
            }
        }
        Value::Array(items) => items.iter().for_each(|v| tags(v, out)),
        _ => {}
    }
}

#[test]
fn the_vectors_cover_every_variant() {
    let mut seen = BTreeSet::new();
    for m in messages() {
        let value: Value = serde_json::from_str(m["json"].as_str().unwrap()).unwrap();
        tags(&value, &mut seen);
    }
    let has = |tag: &str, name: &str| seen.contains(&(tag.to_owned(), name.to_owned()));
    for op in Op::NAMES {
        assert!(has("op", op), "op {op}");
    }
    for (_, r) in requests() {
        assert!(Op::NAMES.contains(&r.op.name()));
    }
    let expect: &[(&str, &[&str])] = &[
        (
            "feed",
            &[
                "projects",
                "sessions",
                "live",
                "inbox",
                "cards",
                "transcript",
                "changes",
            ],
        ),
        (
            "type",
            &[
                // Resp
                "unit",
                "pong",
                "accepted",
                "outcome",
                "models",
                "diff",
                "attached",
                // DoneView
                "session",
                "submitted",
                "answered",
                "deleted",
                "file",
                // Event
                "feed",
                "progress",
                "confirm",
                "term_state",
                "input_refused",
                "model_unconfirmed",
                "resync",
                // SessionAction
                "handoff",
                "compact",
                "rename",
                "auto_name",
                "plan",
                "retry",
                // InboxKind
                "finished",
                "error",
                "attention",
                "exited",
                // Item
                "user",
                "assistant",
                "thinking",
                "tool_call",
                "tool_result",
                "notice",
                "irc",
                "compaction",
                "model_change",
                "exit",
            ],
        ),
        (
            "state",
            &[
                // OutcomeView
                "running",
                "done",
                "failed",
                "rejected",
                "cancelled",
                "uncertain",
                "recorded",
                "unknown",
                "expired",
                // DeliveryView
                "queued",
                "awaiting_confirm",
                "writing",
                "delivered",
                "refused",
                "declined",
                "aborted",
                "stalled",
                "unobserved",
                // Status
                "idle",
                "busy",
                "attention",
                "exited",
            ],
        ),
        (
            "code",
            &[
                // ApiError
                "unsupported",
                "not_allowed",
                "bad_request",
                "not_found",
                "stale",
                "rejected",
                "geometry_owned",
                "too_many_requests",
                "too_large",
                "maintenance",
                "key_reused",
                "expired",
                "failed",
                // RejectedView
                "exited",
                "read_only",
                "detached",
                "busy",
                "no_bracketed_paste",
                "not_injected",
                "empty",
                "no_change",
                "model_unknown",
                "input_held",
                "bad_path",
            ],
        ),
        (
            "reason",
            &[
                "exited",
                "closed",
                "stale",
                "overflow",
                "disconnected",
                "shutdown",
                "detached",
                "not_allowed",
                "too_large",
            ],
        ),
    ];
    for (tag, names) in expect {
        for name in *names {
            assert!(has(tag, name), "{tag} {name}");
        }
    }
}
