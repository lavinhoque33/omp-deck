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
        (
            "diff staged",
            r(
                42,
                None,
                Op::Diff {
                    project: PROJECT.into(),
                    file: "src/main.rs".into(),
                    side: DiffSide::Staged,
                },
            ),
        ),
        (
            "diff untracked",
            r(
                43,
                None,
                Op::Diff {
                    project: PROJECT.into(),
                    file: "docs/".into(),
                    side: DiffSide::Untracked,
                },
            ),
        ),
        (
            "attach input",
            r(
                44,
                None,
                Op::Attach {
                    live: live(),
                    mode: ViewMode::Input,
                },
            ),
        ),
        (
            "subscribe transcript of the session",
            r(
                45,
                None,
                Op::Subscribe {
                    feed: Feed::Transcript {
                        session: SESSION.into(),
                        agent: None,
                        tail: 0,
                    },
                },
            ),
        ),
        (
            "action compact default",
            r(
                46,
                Some(28),
                Op::Action {
                    live: live(),
                    action: SessionAction::Compact {
                        mode: CompactMode::Default,
                        focus: None,
                    },
                },
            ),
        ),
        (
            "action compact soft",
            r(
                47,
                Some(29),
                Op::Action {
                    live: live(),
                    action: SessionAction::Compact {
                        mode: CompactMode::Soft,
                        focus: None,
                    },
                },
            ),
        ),
        (
            "action compact snapcompact",
            r(
                48,
                Some(30),
                Op::Action {
                    live: live(),
                    action: SessionAction::Compact {
                        mode: CompactMode::Snapcompact,
                        focus: None,
                    },
                },
            ),
        ),
        (
            "action handoff with focus",
            r(
                49,
                Some(31),
                Op::Action {
                    live: live(),
                    action: SessionAction::Handoff {
                        focus: Some("the tests".into()),
                    },
                },
            ),
        ),
        (
            "switch_model with level",
            r(
                50,
                Some(32),
                Op::SwitchModel {
                    live: live(),
                    selector: "openai/gpt-5".into(),
                    level: Some("high".into()),
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
        (
            "error not_allowed control",
            err(
                47,
                ApiError::NotAllowed {
                    need: Class::Control,
                },
            ),
        ),
        (
            "error not_allowed view",
            err(48, ApiError::NotAllowed { need: Class::View }),
        ),
        (
            "error stale with the current session",
            err(
                49,
                ApiError::Stale {
                    current: Some(live()),
                },
            ),
        ),
        (
            "outcome done file without a session id",
            done(50, DoneView::File { session: None }),
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
    let session = |status: Status, activity: Activity, requests| LiveSession {
        live: live(),
        kind: LiveKind::Session,
        label: "Fix the parser".into(),
        title: Some("Fix the parser".into()),
        session: Some(SESSION.into()),
        project: Some(PROJECT.into()),
        display: "~/src/app".into(),
        activity,
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
        at_unix_ms: 1_761_000_000_000,
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
                        modified_unix_ms: 1_761_000_000_000,
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
                            Activity::Busy,
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
                                own(DeliveryView::Cancelled {
                                    reason: CancelReason::Exited,
                                }),
                                own(DeliveryView::Cancelled {
                                    reason: CancelReason::Detached,
                                }),
                                own(DeliveryView::Cancelled {
                                    reason: CancelReason::Closing,
                                }),
                                own(DeliveryView::Cancelled {
                                    reason: CancelReason::NotAllowed,
                                }),
                                own(DeliveryView::Unobserved {}),
                            ],
                        ),
                        session(Status::Idle {}, Activity::Idle, vec![]),
                        session(Status::Attention {}, Activity::Attention, vec![]),
                        LiveSession {
                            kind: LiveKind::Tool,
                            session: None,
                            exit_code: Some(0),
                            busy: false,
                            ..session(Status::Exited { clean: true }, Activity::Unknown, vec![])
                        },
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
                    cards: vec![
                        Card {
                            session: SESSION.into(),
                            model: Some("anthropic/claude-sonnet-4".into()),
                            thinking: Some("high".into()),
                            context_tokens: Some(42_000),
                            input_tokens: 1200,
                            output_tokens: 340,
                            cost: Some(0.25),
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
                        },
                        Card {
                            session: "other".into(),
                            model: None,
                            thinking: None,
                            context_tokens: None,
                            input_tokens: 0,
                            output_tokens: 0,
                            cost: None,
                            last_reply: None,
                            last_turn_failed: true,
                            git: None,
                            agents: AgentCounts::default(),
                        },
                    ],
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
                            at_unix_ms: Some(1_761_000_000_000),
                        },
                        Item::Assistant {
                            text: "hello".into(),
                            model: Some("anthropic/claude-sonnet-4".into()),
                            at_unix_ms: None,
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
        (
            "feed changes outside a work tree",
            feed(
                Feed::Changes {
                    project: PROJECT.into(),
                },
                FeedValue::Changes {
                    changes: None,
                    error: Some("not a git repository".into()),
                    truncated: false,
                },
            ),
        ),
    ]
}

fn events() -> Vec<(&'static str, Event)> {
    let refused = |reason| Event::InputRefused { view: 2, reason };
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
            "progress without a total",
            Event::Progress {
                seq: 24,
                stage: "Copying".into(),
                done: 0,
                total: None,
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
            "input_refused not_allowed",
            refused(InputRefusal::NotAllowed),
        ),
        ("input_refused input_held", refused(InputRefusal::InputHeld)),
        ("input_refused exited", refused(InputRefusal::Exited)),
        ("input_refused read_only", refused(InputRefusal::ReadOnly)),
        ("input_refused detached", refused(InputRefusal::Detached)),
        ("input_refused stale", refused(InputRefusal::Stale)),
        ("input_refused rejected", refused(InputRefusal::Rejected)),
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
        ("exited without a code", TermEnd::Exited { code: None }),
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
    // One byte over the message cap: a T_OUTPUT of zeros.
    let mut huge = vec![kind::T_OUTPUT];
    huge.extend_from_slice(&((MAX_APP_MESSAGE + 1 - RECORD_HEADER) as u32).to_be_bytes());
    huge.resize(MAX_APP_MESSAGE + 1, 0);
    vec![
        (
            "empty message",
            Origin::Device,
            String::new(),
            RecordError::Empty,
        ),
        (
            "message over the cap",
            Origin::Gateway,
            hex(&huge),
            RecordError::TooLarge(MAX_APP_MESSAGE + 1),
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
            "unknown device kind from a device",
            Origin::Device,
            "0500000000".into(),
            RecordError::UnknownKind(0x05),
        ),
        (
            "reserved kind 0x00 from a device",
            Origin::Device,
            "0000000000".into(),
            RecordError::UnknownKind(0x00),
        ),
        (
            "reserved kind 0x80 from a gateway",
            Origin::Gateway,
            "8000000000".into(),
            RecordError::UnknownKind(0x80),
        ),
        (
            "unknown gateway-range kind from a device",
            Origin::Device,
            "7f00000000".into(),
            RecordError::WrongDirection(0x7f),
        ),
        (
            "unknown device-range kind from a gateway",
            Origin::Gateway,
            "0f00000000".into(),
            RecordError::WrongDirection(0x0f),
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
            json!({"error": "unsupported", "id": 9}),
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
            "unknown session action type",
            r#"{"id":12,"seq":1,"op":{"op":"action","live":{"key":"k","generation":1},"action":{"type":"undo"}}}"#,
            json!({"error": "unsupported", "id": 12}),
        ),
        (
            "unknown compact mode",
            r#"{"id":13,"seq":1,"op":{"op":"action","live":{"key":"k","generation":1},"action":{"type":"compact","mode":"turbo"}}}"#,
            json!({"error": "unsupported", "id": 13}),
        ),
        (
            "unknown attach mode",
            r#"{"id":14,"op":{"op":"attach","live":{"key":"k","generation":1},"mode":"steer"}}"#,
            json!({"error": "unsupported", "id": 14}),
        ),
        (
            "unknown diff side",
            r#"{"id":15,"op":{"op":"diff","project":"p","file":"f","side":"both"}}"#,
            json!({"error": "unsupported", "id": 15}),
        ),
        (
            "enum value of the wrong type",
            r#"{"id":16,"op":{"op":"diff","project":"p","file":"f","side":1}}"#,
            json!({"error": "bad_request", "id": 16}),
        ),
        (
            "feed without its tag",
            r#"{"id":17,"op":{"op":"subscribe","feed":{"project":"p"}}}"#,
            json!({"error": "bad_request", "id": 17}),
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

/// `(case, origin, bytes, records)`: messages whose unknown gateway kinds a device skips.
fn skipped_records() -> Vec<(&'static str, Origin, Vec<u8>, Vec<Record>)> {
    let reply = Record::Reply(serde_json::to_vec(&replies()[0].1).unwrap());
    let output = Record::Output {
        view: 2,
        bytes: b"ok".to_vec(),
    };
    let mut mixed = encode(std::slice::from_ref(&reply)).unwrap();
    mixed.extend_from_slice(&[0x13, 0, 0, 0, 2, 0xab, 0xcd]);
    mixed.extend(encode(std::slice::from_ref(&output)).unwrap());
    mixed.extend_from_slice(&[0x7f, 0, 0, 0, 0]);
    vec![
        (
            "unknown gateway kinds between known ones",
            Origin::Gateway,
            mixed,
            vec![reply, output],
        ),
        (
            "only an unknown gateway kind",
            Origin::Gateway,
            vec![0x30, 0, 0, 0, 1, 0],
            vec![],
        ),
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
    let skipped: Vec<Value> = skipped_records()
        .into_iter()
        .map(|(case, origin, bytes, records)| {
            json!({
                "case": case,
                "origin": origin_name(origin),
                "hex": hex(&bytes),
                "records": records.iter().map(record_json).collect::<Vec<_>>(),
            })
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
        "description": "Device application protocol v1 (deck_remote::api) shared with pwa/src/remote. records: whole application messages (hex) and their records; skipped_records: messages a device decodes to the listed records, skipping unknown gateway kinds; rejected_records: messages refused whole; messages: the canonical JSON text of every message variant (decode equals the value, encode equals the text; None is null); rejected_requests: REQUEST bodies answered unsupported/bad_request (by id) or closing the session (unreadable). Generated by crates/deck-remote/tests/api.rs.",
        "protocol": PROTOCOL,
        "records": records,
        "skipped_records": skipped,
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
    let skipped = file["skipped_records"].as_array().unwrap();
    let expected = skipped_records();
    assert_eq!(skipped.len(), expected.len());
    for (v, (case, origin, _, records)) in skipped.iter().zip(expected) {
        let bytes = unhex(v["hex"].as_str().unwrap());
        assert_eq!(decode(&bytes, origin).unwrap(), records, "{case}");
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

fn request_error_name(e: &RequestError) -> &'static str {
    match e {
        RequestError::Unreadable => "unreadable",
        RequestError::Unsupported { .. } => "unsupported",
        RequestError::Bad { .. } => "bad_request",
    }
}

fn request_error_id(e: &RequestError) -> Option<u64> {
    match e {
        RequestError::Unreadable => None,
        RequestError::Unsupported { id } | RequestError::Bad { id, .. } => Some(*id),
    }
}

#[test]
fn rejected_requests_are_classified() {
    for (case, text, expect) in rejected_requests() {
        let got = Request::parse(text.as_bytes()).unwrap_err();
        assert_eq!(request_error_name(&got), expect["error"], "{case}");
        assert_eq!(request_error_id(&got), expect["id"].as_u64(), "{case}");
    }
}

/// Every string inside each v1 request's op, replaced by one no version knows. Where serde
/// refuses it as an unknown variant it was a tag or an enum value, which must answer
/// `unsupported`; any other refusal `bad_request`. Fails when an op gains a nested enum that
/// `Request::parse` does not classify.
#[test]
fn unknown_tags_and_enum_values_anywhere_in_an_op_are_unsupported() {
    fn strings(v: &Value, path: String, out: &mut Vec<String>) {
        match v {
            Value::String(_) => out.push(path),
            Value::Object(m) => {
                for (k, v) in m {
                    strings(v, format!("{path}/{k}"), out);
                }
            }
            Value::Array(a) => {
                for (i, v) in a.iter().enumerate() {
                    strings(v, format!("{path}/{i}"), out);
                }
            }
            _ => {}
        }
    }
    let mut unsupported = BTreeSet::new();
    for (case, request) in requests() {
        let value = serde_json::to_value(&request).unwrap();
        let mut paths = Vec::new();
        strings(&value["op"], "/op".into(), &mut paths);
        for path in paths {
            let mut probe = value.clone();
            *probe.pointer_mut(&path).unwrap() = json!("zz_unknown");
            let body = serde_json::to_vec(&probe).unwrap();
            let expected = match serde_json::from_slice::<Request>(&body) {
                Ok(_) => continue,
                Err(e) if e.to_string().starts_with("unknown variant") => "unsupported",
                Err(_) => "bad_request",
            };
            let got = Request::parse(&body).unwrap_err();
            assert_eq!(request_error_name(&got), expected, "{case} {path}");
            assert_eq!(request_error_id(&got), Some(request.id), "{case} {path}");
            if expected == "unsupported" {
                let field = path.rsplit('/').next().unwrap_or_default().to_owned();
                unsupported.insert(field);
            }
        }
    }
    // The op tag, the feed tag, the action type and the three plain enums.
    let fields: BTreeSet<String> = ["op", "feed", "type", "mode", "side"]
        .map(String::from)
        .into();
    assert_eq!(unsupported, fields);
}

// ---- variant coverage ----------------------------------------------------------------------------

/// `all_variants!(Type { pattern => value, … })`: one value per variant. The patterns form an
/// exhaustive `match`, so a new variant does not compile until it is listed here, and
/// `the_vectors_cover_every_variant` then fails until `api-v1.json` carries it.
macro_rules! all_variants {
    ($ty:ty { $($pat:pat => $value:expr),+ $(,)? }) => {{
        fn exhaustive(v: &$ty) {
            match v {
                $($pat)|+ => {}
            }
        }
        let all: Vec<$ty> = vec![$({
            let v: $ty = $value;
            assert!(matches!(&v, $pat), "{} is not {}", stringify!($value), stringify!($pat));
            exhaustive(&v);
            v
        }),+];
        all
    }};
}

/// An api enum and every one of its variants.
trait Variants: Sized + serde::Serialize + std::fmt::Debug {
    const TYPE: &'static str;
    /// The tag field of an internally tagged enum; `None`: a plain string, or externally
    /// tagged (`{ "variant": … }`).
    const TAG: Option<&'static str>;
    fn all_variants() -> Vec<Self>;
}

macro_rules! variants {
    ($ty:ident, $tag:expr, { $($body:tt)+ }) => {
        impl Variants for $ty {
            const TYPE: &'static str = stringify!($ty);
            const TAG: Option<&'static str> = $tag;
            fn all_variants() -> Vec<$ty> {
                all_variants!($ty { $($body)+ })
            }
        }
    };
}

/// A plain string enum.
macro_rules! plain {
    ($ty:ident: $($v:ident),+ $(,)?) => {
        variants!($ty, None, { $($ty::$v => $ty::$v),+ });
    };
}

plain!(Class: View, Control, Manage);
plain!(ViewMode: Observe, Input, Control);
plain!(DiffSide: Staged, Unstaged, Untracked);
plain!(CompactMode: Default, Soft, Snapcompact, Focus);
plain!(LiveKind: Session, Tool);
plain!(Activity: Unknown, Idle, Busy, Attention);
plain!(ChangeKind: Modified, Added, Deleted, Renamed, Copied, TypeChanged);
plain!(Refusal: Precondition, DialogOpen, NoBracketedPaste);
plain!(CancelReason: Exited, Detached, Closing, SessionChanged, NotAllowed);
plain!(InputRefusal: NotAllowed, InputHeld, Exited, ReadOnly, Detached, Stale, Rejected);

variants!(SessionAction, Some("type"), {
    SessionAction::Handoff { .. } => SessionAction::Handoff { focus: None },
    SessionAction::Compact { .. } => SessionAction::Compact {
        mode: CompactMode::Focus,
        focus: None,
    },
    SessionAction::Rename { .. } => SessionAction::Rename { name: String::new() },
    SessionAction::AutoName {} => SessionAction::AutoName {},
    SessionAction::Plan {} => SessionAction::Plan {},
    SessionAction::Retry {} => SessionAction::Retry {},
});

variants!(Op, Some("op"), {
    Op::Subscribe { .. } => Op::Subscribe { feed: Feed::Live {} },
    Op::Unsubscribe { .. } => Op::Unsubscribe { feed: Feed::Live {} },
    Op::Outcome { .. } => Op::Outcome { seq: 1 },
    Op::Models {} => Op::Models {},
    Op::Diff { .. } => Op::Diff {
        project: String::new(),
        file: String::new(),
        side: DiffSide::Staged,
    },
    Op::Attach { .. } => Op::Attach {
        live: live(),
        mode: ViewMode::Observe,
    },
    Op::Detach { .. } => Op::Detach { view: 1 },
    Op::Ping {} => Op::Ping {},
    Op::NewSession { .. } => Op::NewSession { project: String::new() },
    Op::Resume { .. } => Op::Resume { session: String::new() },
    Op::Restart { .. } => Op::Restart { live: live() },
    Op::Stop { .. } => Op::Stop { live: live() },
    Op::Kill { .. } => Op::Kill { live: live() },
    Op::Prompt { .. } => Op::Prompt {
        live: live(),
        text: String::new(),
    },
    Op::Interrupt { .. } => Op::Interrupt { live: live() },
    Op::SwitchModel { .. } => Op::SwitchModel {
        live: live(),
        selector: String::new(),
        level: None,
    },
    Op::SetEffort { .. } => Op::SetEffort {
        live: live(),
        level: String::new(),
    },
    Op::Action { .. } => Op::Action {
        live: live(),
        action: SessionAction::Plan {},
    },
    Op::Answer { .. } => Op::Answer {
        live: live(),
        request: 1,
        proceed: true,
    },
    Op::Cancel { .. } => Op::Cancel { seq: 1 },
    Op::InboxOpen { .. } => Op::InboxOpen { live: live() },
    Op::InboxDismiss { .. } => Op::InboxDismiss { live: live() },
    Op::InboxClear {} => Op::InboxClear {},
    Op::Stage { .. } => Op::Stage {
        project: String::new(),
        files: vec![],
    },
    Op::Unstage { .. } => Op::Unstage {
        project: String::new(),
        files: vec![],
    },
    Op::DeleteSessions { .. } => Op::DeleteSessions { sessions: vec![] },
    Op::CopySession { .. } => Op::CopySession {
        session: String::new(),
        project: String::new(),
    },
    Op::MoveSession { .. } => Op::MoveSession {
        session: String::new(),
        project: String::new(),
    },
    Op::Discard { .. } => Op::Discard {
        project: String::new(),
        files: vec![],
    },
    Op::DeleteUntracked { .. } => Op::DeleteUntracked {
        project: String::new(),
        files: vec![],
    },
});

variants!(Feed, Some("feed"), {
    Feed::Projects {} => Feed::Projects {},
    Feed::Sessions { .. } => Feed::Sessions {
        project: String::new(),
        limit: 1,
    },
    Feed::Live {} => Feed::Live {},
    Feed::Inbox {} => Feed::Inbox {},
    Feed::Cards { .. } => Feed::Cards { sessions: vec![] },
    Feed::Transcript { .. } => Feed::Transcript {
        session: String::new(),
        agent: None,
        tail: 0,
    },
    Feed::Changes { .. } => Feed::Changes { project: String::new() },
});

variants!(FeedValue, Some("feed"), {
    FeedValue::Projects { .. } => FeedValue::Projects {
        projects: vec![],
        truncated: false,
    },
    FeedValue::Sessions { .. } => FeedValue::Sessions {
        project: String::new(),
        total: 0,
        sessions: vec![],
        truncated: false,
    },
    FeedValue::Live { .. } => FeedValue::Live {
        sessions: vec![],
        truncated: false,
    },
    FeedValue::Inbox { .. } => FeedValue::Inbox {
        inbox: Inbox {
            entries: vec![],
            warns: false,
            badge: None,
        },
        truncated: false,
    },
    FeedValue::Cards { .. } => FeedValue::Cards {
        cards: vec![],
        truncated: false,
    },
    FeedValue::Transcript { .. } => FeedValue::Transcript {
        start: 0,
        items: vec![],
        reset: false,
    },
    FeedValue::Changes { .. } => FeedValue::Changes {
        changes: None,
        error: None,
        truncated: false,
    },
});

variants!(Status, Some("state"), {
    Status::Idle {} => Status::Idle {},
    Status::Busy {} => Status::Busy {},
    Status::Attention {} => Status::Attention {},
    Status::Exited { .. } => Status::Exited { clean: true },
});

variants!(InboxKind, Some("type"), {
    InboxKind::Finished {} => InboxKind::Finished {},
    InboxKind::Error { .. } => InboxKind::Error { message: None },
    InboxKind::Attention {} => InboxKind::Attention {},
    InboxKind::Exited { .. } => InboxKind::Exited { code: None },
});

variants!(Item, Some("type"), {
    Item::User { .. } => Item::User {
        text: String::new(),
        from_agent: false,
        at_unix_ms: None,
    },
    Item::Assistant { .. } => Item::Assistant {
        text: String::new(),
        model: None,
        at_unix_ms: None,
    },
    Item::Thinking { .. } => Item::Thinking { text: String::new() },
    Item::ToolCall { .. } => Item::ToolCall {
        call_id: String::new(),
        name: String::new(),
        intent: None,
        summary: String::new(),
        args: String::new(),
    },
    Item::ToolResult { .. } => Item::ToolResult {
        call_id: String::new(),
        name: String::new(),
        text: String::new(),
        is_error: false,
        truncated: false,
    },
    Item::Notice { .. } => Item::Notice {
        kind: String::new(),
        text: String::new(),
    },
    Item::Irc { .. } => Item::Irc {
        from: None,
        agent: None,
        text: String::new(),
        note: None,
    },
    Item::Compaction { .. } => Item::Compaction {
        tokens_before: None,
        tokens_after: None,
        summary: String::new(),
    },
    Item::ModelChange { .. } => Item::ModelChange { model: String::new() },
    Item::Error { .. } => Item::Error { message: String::new() },
    Item::Exit { .. } => Item::Exit { reason: String::new() },
});

variants!(ReplyResult, None, {
    ReplyResult::Ok(_) => ReplyResult::Ok(Resp::Unit {}),
    ReplyResult::Err(_) => ReplyResult::Err(ApiError::Unsupported {}),
});

variants!(Resp, Some("type"), {
    Resp::Unit {} => Resp::Unit {},
    Resp::Pong {} => Resp::Pong {},
    Resp::Accepted { .. } => Resp::Accepted {
        seq: 1,
        duplicate: false,
    },
    Resp::Outcome { .. } => Resp::Outcome {
        outcome: OutcomeView::Running {},
    },
    Resp::Models { .. } => Resp::Models {
        models: vec![],
        truncated: false,
    },
    Resp::Diff { .. } => Resp::Diff {
        diff: Diff {
            text: String::new(),
            binary: false,
            truncated: false,
            added: 0,
            removed: 0,
        },
    },
    Resp::Attached { .. } => Resp::Attached {
        view: 1,
        mode: ViewMode::Observe,
    },
});

variants!(OutcomeView, Some("state"), {
    OutcomeView::Running {} => OutcomeView::Running {},
    OutcomeView::Done { .. } => OutcomeView::Done {
        result: DoneView::Unit {},
    },
    OutcomeView::Failed { .. } => OutcomeView::Failed { message: String::new() },
    OutcomeView::Rejected { .. } => OutcomeView::Rejected {
        reason: RejectedView::Busy {},
    },
    OutcomeView::Cancelled {} => OutcomeView::Cancelled {},
    OutcomeView::Uncertain {} => OutcomeView::Uncertain {},
    OutcomeView::Recorded { .. } => OutcomeView::Recorded { succeeded: true },
    OutcomeView::Unknown {} => OutcomeView::Unknown {},
    OutcomeView::Expired {} => OutcomeView::Expired {},
});

variants!(DoneView, Some("type"), {
    DoneView::Unit {} => DoneView::Unit {},
    DoneView::Session { .. } => DoneView::Session { live: live() },
    DoneView::Submitted { .. } => DoneView::Submitted {
        request: 1,
        label: String::new(),
        delivery: DeliveryView::Queued {},
    },
    DoneView::Answered { .. } => DoneView::Answered { applied: true },
    DoneView::Deleted { .. } => DoneView::Deleted { results: vec![] },
    DoneView::File { .. } => DoneView::File { session: None },
});

variants!(RejectedView, Some("code"), {
    RejectedView::Stale { .. } => RejectedView::Stale { current: None },
    RejectedView::Exited {} => RejectedView::Exited {},
    RejectedView::ReadOnly {} => RejectedView::ReadOnly {},
    RejectedView::Detached {} => RejectedView::Detached {},
    RejectedView::Busy {} => RejectedView::Busy {},
    RejectedView::NoBracketedPaste {} => RejectedView::NoBracketedPaste {},
    RejectedView::NotInjected {} => RejectedView::NotInjected {},
    RejectedView::Empty {} => RejectedView::Empty {},
    RejectedView::NoChange {} => RejectedView::NoChange {},
    RejectedView::ModelUnknown {} => RejectedView::ModelUnknown {},
    RejectedView::KeyReused {} => RejectedView::KeyReused {},
    RejectedView::Expired {} => RejectedView::Expired {},
    RejectedView::InputHeld {} => RejectedView::InputHeld {},
    RejectedView::BadPath {} => RejectedView::BadPath {},
});

variants!(DeliveryView, Some("state"), {
    DeliveryView::Queued {} => DeliveryView::Queued {},
    DeliveryView::AwaitingConfirm { .. } => DeliveryView::AwaitingConfirm { draft: String::new() },
    DeliveryView::Writing {} => DeliveryView::Writing {},
    DeliveryView::Delivered {} => DeliveryView::Delivered {},
    DeliveryView::Refused { .. } => DeliveryView::Refused {
        reason: Refusal::Precondition,
    },
    DeliveryView::Declined {} => DeliveryView::Declined {},
    DeliveryView::Aborted {} => DeliveryView::Aborted {},
    DeliveryView::Stalled {} => DeliveryView::Stalled {},
    DeliveryView::Cancelled { .. } => DeliveryView::Cancelled {
        reason: CancelReason::Closing,
    },
    DeliveryView::Unobserved {} => DeliveryView::Unobserved {},
});

variants!(ApiError, Some("code"), {
    ApiError::Unsupported {} => ApiError::Unsupported {},
    ApiError::NotAllowed { .. } => ApiError::NotAllowed {
        need: Class::Control,
    },
    ApiError::BadRequest { .. } => ApiError::BadRequest { message: String::new() },
    ApiError::NotFound { .. } => ApiError::NotFound { what: String::new() },
    ApiError::Stale { .. } => ApiError::Stale { current: None },
    ApiError::Rejected { .. } => ApiError::Rejected {
        reason: RejectedView::Busy {},
    },
    ApiError::GeometryOwned {} => ApiError::GeometryOwned {},
    ApiError::TooManyRequests {} => ApiError::TooManyRequests {},
    ApiError::TooLarge {} => ApiError::TooLarge {},
    ApiError::Maintenance {} => ApiError::Maintenance {},
    ApiError::KeyReused {} => ApiError::KeyReused {},
    ApiError::Expired {} => ApiError::Expired {},
    ApiError::Failed { .. } => ApiError::Failed { message: String::new() },
});

variants!(Event, Some("type"), {
    Event::Feed { .. } => Event::Feed {
        feed: Feed::Live {},
        value: FeedValue::Live {
            sessions: vec![],
            truncated: false,
        },
    },
    Event::Outcome { .. } => Event::Outcome {
        seq: 1,
        outcome: OutcomeView::Running {},
    },
    Event::Progress { .. } => Event::Progress {
        seq: 1,
        stage: String::new(),
        done: 0,
        total: None,
    },
    Event::Confirm { .. } => Event::Confirm {
        live: live(),
        request: 1,
        label: String::new(),
        draft: String::new(),
    },
    Event::TermState { .. } => Event::TermState {
        view: 1,
        input_held: false,
    },
    Event::InputRefused { .. } => Event::InputRefused {
        view: 1,
        reason: InputRefusal::Stale,
    },
    Event::ModelUnconfirmed { .. } => Event::ModelUnconfirmed {
        live: live(),
        selector: None,
        level: None,
    },
    Event::Resync {} => Event::Resync {},
});

variants!(TermEnd, Some("reason"), {
    TermEnd::Exited { .. } => TermEnd::Exited { code: None },
    TermEnd::Closed {} => TermEnd::Closed {},
    TermEnd::Stale {} => TermEnd::Stale {},
    TermEnd::Overflow {} => TermEnd::Overflow {},
    TermEnd::Disconnected {} => TermEnd::Disconnected {},
    TermEnd::Shutdown {} => TermEnd::Shutdown {},
    TermEnd::Detached {} => TermEnd::Detached {},
    TermEnd::NotAllowed {} => TermEnd::NotAllowed {},
    TermEnd::TooLarge {} => TermEnd::TooLarge {},
});

/// A variant's tag or string.
fn variant_name<T: Variants>(v: &T) -> String {
    match (T::TAG, serde_json::to_value(v).unwrap()) {
        (None, Value::String(s)) => s,
        (Some(tag), Value::Object(m)) => m[tag].as_str().unwrap().to_owned(),
        (None, Value::Object(m)) if m.len() == 1 => m.keys().next().unwrap().clone(),
        (_, other) => panic!("{} serializes as {other}", T::TYPE),
    }
}

/// `(type, variant)` pairs found in the vectors.
#[derive(Default)]
struct Seen(BTreeSet<(&'static str, String)>);

impl Seen {
    fn add<T: Variants>(&mut self, v: &T) {
        self.0.insert((T::TYPE, variant_name(v)));
    }

    /// Asserts every variant of `T` was seen; its names in declaration order.
    fn covered<T: Variants>(&self) -> Vec<String> {
        let names: Vec<String> = T::all_variants().iter().map(variant_name).collect();
        for (i, name) in names.iter().enumerate() {
            assert!(!names[..i].contains(name), "{} {name} twice", T::TYPE);
            assert!(
                self.0.contains(&(T::TYPE, name.clone())),
                "api-v1.json has no {} {name}",
                T::TYPE
            );
        }
        names
    }

    fn request(&mut self, r: &Request) {
        self.add(&r.op);
        match &r.op {
            Op::Subscribe { feed } | Op::Unsubscribe { feed } => self.add(feed),
            Op::Diff { side, .. } => self.add(side),
            Op::Attach { mode, .. } => self.add(mode),
            Op::Action { action, .. } => {
                self.add(action);
                if let SessionAction::Compact { mode, .. } = action {
                    self.add(mode);
                }
            }
            _ => {}
        }
    }

    fn reply(&mut self, r: &Reply) {
        self.add(&r.result);
        match &r.result {
            ReplyResult::Ok(resp) => {
                self.add(resp);
                match resp {
                    Resp::Outcome { outcome } => self.outcome(outcome),
                    Resp::Attached { mode, .. } => self.add(mode),
                    _ => {}
                }
            }
            ReplyResult::Err(e) => {
                self.add(e);
                match e {
                    ApiError::NotAllowed { need } => self.add(need),
                    ApiError::Rejected { reason } => self.add(reason),
                    _ => {}
                }
            }
        }
    }

    fn outcome(&mut self, o: &OutcomeView) {
        self.add(o);
        match o {
            OutcomeView::Done { result } => {
                self.add(result);
                if let DoneView::Submitted { delivery, .. } = result {
                    self.delivery(delivery);
                }
            }
            OutcomeView::Rejected { reason } => self.add(reason),
            _ => {}
        }
    }

    fn delivery(&mut self, d: &DeliveryView) {
        self.add(d);
        match d {
            DeliveryView::Refused { reason } => self.add(reason),
            DeliveryView::Cancelled { reason } => self.add(reason),
            _ => {}
        }
    }

    fn event(&mut self, e: &Event) {
        self.add(e);
        match e {
            Event::Feed { feed, value } => {
                self.add(feed);
                self.value(value);
            }
            Event::Outcome { outcome, .. } => self.outcome(outcome),
            Event::InputRefused { reason, .. } => self.add(reason),
            _ => {}
        }
    }

    fn value(&mut self, v: &FeedValue) {
        self.add(v);
        match v {
            FeedValue::Live { sessions, .. } => {
                for s in sessions {
                    self.add(&s.kind);
                    self.add(&s.activity);
                    self.add(&s.status);
                    for r in &s.requests {
                        self.delivery(&r.delivery);
                    }
                }
            }
            FeedValue::Inbox { inbox, .. } => {
                for e in &inbox.entries {
                    self.add(&e.kind);
                }
            }
            FeedValue::Transcript { items, .. } => {
                for i in items {
                    self.add(i);
                }
            }
            FeedValue::Changes {
                changes: Some(c), ..
            } => {
                for f in &c.files {
                    for k in [f.staged, f.unstaged].into_iter().flatten() {
                        self.add(&k);
                    }
                }
            }
            _ => {}
        }
    }
}

/// Per type, every variant (tag or string) appears in `api-v1.json`.
#[test]
fn the_vectors_cover_every_variant() {
    let file: Value = serde_json::from_str(&std::fs::read_to_string(PATH).unwrap()).unwrap();
    let mut seen = Seen::default();
    for m in file["messages"].as_array().unwrap() {
        let text = m["json"].as_str().unwrap();
        match m["type"].as_str().unwrap() {
            "request" => seen.request(&serde_json::from_str(text).unwrap()),
            "reply" => seen.reply(&serde_json::from_str(text).unwrap()),
            "event" => seen.event(&serde_json::from_str(text).unwrap()),
            "welcome" => {}
            "term_end" => seen.add(&serde_json::from_str::<TermEnd>(text).unwrap()),
            other => panic!("message type {other}"),
        }
    }
    seen.covered::<Class>();
    assert_eq!(seen.covered::<ViewMode>(), ViewMode::NAMES);
    assert_eq!(seen.covered::<DiffSide>(), DiffSide::NAMES);
    assert_eq!(seen.covered::<CompactMode>(), CompactMode::NAMES);
    seen.covered::<LiveKind>();
    seen.covered::<Activity>();
    seen.covered::<ChangeKind>();
    seen.covered::<Refusal>();
    seen.covered::<CancelReason>();
    seen.covered::<InputRefusal>();
    assert_eq!(seen.covered::<SessionAction>(), SessionAction::NAMES);
    assert_eq!(seen.covered::<Op>(), Op::NAMES);
    assert_eq!(seen.covered::<Feed>(), Feed::NAMES);
    seen.covered::<FeedValue>();
    seen.covered::<Status>();
    seen.covered::<InboxKind>();
    seen.covered::<Item>();
    seen.covered::<ReplyResult>();
    seen.covered::<Resp>();
    seen.covered::<OutcomeView>();
    seen.covered::<DoneView>();
    seen.covered::<RejectedView>();
    seen.covered::<DeliveryView>();
    seen.covered::<ApiError>();
    seen.covered::<Event>();
    seen.covered::<TermEnd>();
    for (_, r) in requests() {
        assert_eq!(serde_json::to_value(&r.op).unwrap()["op"], r.op.name());
    }

    // The binary layer and request classification, by their vector names.
    let names = |section: &str, field: &str| -> BTreeSet<String> {
        file[section]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v[field].as_str().unwrap().to_owned())
            .collect()
    };
    let mut kinds = BTreeSet::new();
    for section in ["records", "skipped_records"] {
        for v in file[section].as_array().unwrap() {
            for r in v["records"].as_array().unwrap() {
                kinds.insert(r["kind"].as_str().unwrap().to_owned());
            }
        }
    }
    let records = all_variants!(Record {
        Record::Request(_) => Record::Request(vec![]),
        Record::Ack { .. } => Record::Ack { messages: 0 },
        Record::Input { .. } => Record::Input {
            view: 0,
            ctrl_c: false,
            bytes: vec![],
        },
        Record::Resize { .. } => Record::Resize {
            view: 0,
            cols: 0,
            rows: 0,
        },
        Record::Welcome(_) => Record::Welcome(vec![]),
        Record::Reply(_) => Record::Reply(vec![]),
        Record::Event(_) => Record::Event(vec![]),
        Record::Snapshot { .. } => Record::Snapshot {
            view: 0,
            last: false,
            bytes: vec![],
        },
        Record::Output { .. } => Record::Output {
            view: 0,
            bytes: vec![],
        },
        Record::TermResize { .. } => Record::TermResize {
            view: 0,
            cols: 0,
            rows: 0,
        },
        Record::TermEnd { .. } => Record::TermEnd {
            view: 0,
            end: vec![],
        },
    });
    for r in &records {
        let kind = record_json(r)["kind"].as_str().unwrap().to_owned();
        assert!(kinds.contains(&kind), "api-v1.json has no {kind} record");
    }
    let errors = names("rejected_records", "error");
    for e in all_variants!(RecordError {
        RecordError::Empty => RecordError::Empty,
        RecordError::TooLarge(_) => RecordError::TooLarge(0),
        RecordError::Truncated => RecordError::Truncated,
        RecordError::UnknownKind(_) => RecordError::UnknownKind(0),
        RecordError::WrongDirection(_) => RecordError::WrongDirection(0),
        RecordError::Malformed(_) => RecordError::Malformed(0),
        RecordError::Oversized { .. } => RecordError::Oversized {
            kind: 0,
            len: 0,
            max: 0,
        },
    }) {
        assert!(errors.contains(error_name(&e)), "api-v1.json has no {e:?}");
    }
    let refusals = names("rejected_requests", "error");
    for e in all_variants!(RequestError {
        RequestError::Unreadable => RequestError::Unreadable,
        RequestError::Unsupported { .. } => RequestError::Unsupported { id: 0 },
        RequestError::Bad { .. } => RequestError::Bad {
            id: 0,
            message: String::new(),
        },
    }) {
        assert!(
            refusals.contains(request_error_name(&e)),
            "api-v1.json has no {e:?}"
        );
    }
    let mut origins = names("records", "origin");
    origins.extend(names("rejected_records", "origin"));
    for o in all_variants!(Origin {
        Origin::Device => Origin::Device,
        Origin::Gateway => Origin::Gateway,
    }) {
        assert!(origins.contains(origin_name(o)), "api-v1.json has no {o:?}");
    }
}
