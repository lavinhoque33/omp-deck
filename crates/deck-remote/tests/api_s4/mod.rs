//! The slice-4 additions against `tests/vectors/api-s4.json` (`docs/notes/mobile-pwa.md`, "Wire
//! additions"), which the PWA tests read too: one canonical JSON text per new message variant,
//! enum value and additive field, and the new request refusals. Same format as `api-v1.json`'s
//! `messages`/`rejected_requests`, plus `features`. Regenerated with the v1 file:
//! `OMPDECK_WRITE_VECTORS=1 cargo test -p deck-remote --test api`

use super::*;

pub const PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/vectors/api-s4.json");

const DIR: &str = "f-0123456789abcdef";
const RUNNING: &str = "r-0123456789abcdef";
const AT: u64 = 1_761_000_000_000;

fn s(text: &str) -> String {
    text.to_owned()
}

pub fn requests() -> Vec<(&'static str, Request)> {
    let view = |id: u64, op: Op| Request { id, seq: None, op };
    let journaled = |id: u64, op: Op| Request {
        id,
        seq: Some(id),
        op,
    };
    let mut out = vec![
        (
            "last_reply",
            view(
                101,
                Op::LastReply {
                    session: SESSION.into(),
                },
            ),
        ),
        (
            "export",
            view(
                102,
                Op::Export {
                    session: SESSION.into(),
                },
            ),
        ),
        (
            "blob of a subagent",
            view(
                103,
                Op::Blob {
                    session: SESSION.into(),
                    agent: Some(s("CoreStatus")),
                    blob: s("0123456789abcdef"),
                },
            ),
        ),
        (
            "blob",
            view(
                104,
                Op::Blob {
                    session: SESSION.into(),
                    agent: None,
                    blob: s("0123456789abcdef"),
                },
            ),
        ),
        (
            "download",
            view(
                105,
                Op::Download {
                    download: s("x-1"),
                    offset: 524_288,
                },
            ),
        ),
    ];
    for (case, id, filter, cursor) in [
        ("tree conversation", 106, TreeFilter::Conversation, None),
        ("tree default", 107, TreeFilter::Default, Some(s("c-500"))),
        ("tree all", 108, TreeFilter::All, None),
        ("tree labeled", 109, TreeFilter::Labeled, None),
    ] {
        out.push((
            case,
            view(
                id,
                Op::Tree {
                    session: SESSION.into(),
                    filter,
                    cursor,
                    limit: 500,
                },
            ),
        ));
    }
    out.extend([
        ("browse roots", view(110, Op::Browse { dir: None })),
        (
            "browse a folder",
            view(
                111,
                Op::Browse {
                    dir: Some(s(DIR)),
                },
            ),
        ),
        ("usage_activity", view(112, Op::UsageActivity { days: 30 })),
        ("usage_refresh", view(113, Op::UsageRefresh {})),
        ("omp_settings", view(114, Op::OmpSettings {})),
        ("drive_backups", view(115, Op::DriveBackups {})),
        ("sync_backups", view(116, Op::SyncBackups { project: None })),
        (
            "sync_backups of a project",
            view(
                117,
                Op::SyncBackups {
                    project: Some(PROJECT.into()),
                },
            ),
        ),
        ("presence", view(118, Op::Presence { visible: true })),
        (
            "push_subscribe",
            view(
                119,
                Op::PushSubscribe {
                    endpoint: s("https://fcm.googleapis.com/fcm/send/abc"),
                    p256dh: s("BNcRdreALRFXTkOOUHK1EtK2wtaz5Ry4YfYCA_0QTpQtUbVlUls0VJXg7A8u-Ts1XbjhazAkj7I99e8QcYP7DkM"),
                    auth: s("tBHItJI5svbpez7KI4CCXg"),
                },
            ),
        ),
        ("push_unsubscribe", view(120, Op::PushUnsubscribe {})),
        (
            "push_prefs",
            view(
                121,
                Op::PushPrefs {
                    prefs: PushPrefs {
                        turn_finished: true,
                        attention: true,
                        ended: false,
                        show_titles: true,
                    },
                },
            ),
        ),
        (
            "device_rename",
            view(122, Op::DeviceRename { name: s("Pixel 9") }),
        ),
        ("unpair", view(123, Op::Unpair {})),
        (
            "upload_begin",
            view(
                124,
                Op::UploadBegin {
                    name: s("shot.png"),
                    mime: s("image/png"),
                    size: 48_213,
                },
            ),
        ),
        (
            "upload_chunk",
            view(
                125,
                Op::UploadChunk {
                    upload: s("x-2"),
                    offset: 0,
                    data: s("iVBORw0KGgo="),
                },
            ),
        ),
        ("upload_cancel", view(126, Op::UploadCancel { upload: s("x-2") })),
    ]);
    for (case, id, mode, attachments) in [
        (
            "compose send with an attachment",
            127,
            ComposeMode::Send,
            vec![s("x-2")],
        ),
        ("compose steer", 128, ComposeMode::Steer, vec![]),
        ("compose follow_up", 129, ComposeMode::FollowUp, vec![]),
        ("compose insert", 130, ComposeMode::Insert, vec![]),
    ] {
        out.push((
            case,
            journaled(
                id,
                Op::Compose {
                    live: live(),
                    text: s("look at this"),
                    attachments,
                    mode,
                },
            ),
        ));
    }
    let comment = |side, start, end| ReviewComment {
        file: s("src/main.rs"),
        side,
        start,
        end,
        text: s("why?"),
    };
    out.extend([
        (
            "review sent",
            journaled(
                131,
                Op::Review {
                    live: live(),
                    comments: vec![
                        comment(DiffSide::Staged, 3, 3),
                        comment(DiffSide::Unstaged, 10, 14),
                        comment(DiffSide::Untracked, 1, 2),
                    ],
                    general: Some(s("Mostly fine.")),
                    send: true,
                },
            ),
        ),
        (
            "review inserted",
            journaled(
                132,
                Op::Review {
                    live: live(),
                    comments: vec![comment(DiffSide::Unstaged, 7, 7)],
                    general: None,
                    send: false,
                },
            ),
        ),
    ]);
    let answer = |id: u64, value: UiAnswer| {
        journaled(
            id,
            Op::AnswerUi {
                live: live(),
                request: s("u-17"),
                value,
            },
        )
    };
    out.extend([
        ("answer choice", answer(133, UiAnswer::Choice { index: 1 })),
        (
            "answer choices",
            answer(
                134,
                UiAnswer::Choices {
                    indices: vec![0, 2],
                },
            ),
        ),
        (
            "answer text",
            answer(135, UiAnswer::Text { text: s("main") }),
        ),
        (
            "answer confirm",
            answer(136, UiAnswer::Confirm { yes: false }),
        ),
        (
            "answer ask",
            answer(
                137,
                UiAnswer::Ask {
                    answers: vec![
                        UiAskAnswer {
                            id: s("scope"),
                            selected: vec![0],
                            custom: None,
                        },
                        UiAskAnswer {
                            id: s("extras"),
                            selected: vec![],
                            custom: Some(s("both")),
                        },
                    ],
                },
            ),
        ),
        (
            "answer approve once",
            answer(
                138,
                UiAnswer::Approve {
                    decision: ApproveDecision::Once {},
                },
            ),
        ),
        (
            "answer approve session",
            answer(
                139,
                UiAnswer::Approve {
                    decision: ApproveDecision::Session {},
                },
            ),
        ),
        (
            "answer approve deny",
            answer(
                140,
                UiAnswer::Approve {
                    decision: ApproveDecision::Deny {},
                },
            ),
        ),
        (
            "answer approve grant",
            answer(
                141,
                UiAnswer::Approve {
                    decision: ApproveDecision::Grant {
                        index: 0,
                        always: true,
                    },
                },
            ),
        ),
        ("answer cancel", answer(142, UiAnswer::Cancel {})),
        (
            "agent_kill",
            journaled(
                143,
                Op::AgentKill {
                    live: live(),
                    agent: s("CoreStatus"),
                },
            ),
        ),
        (
            "agent_revive",
            journaled(
                144,
                Op::AgentRevive {
                    live: live(),
                    agent: s("CoreStatus"),
                },
            ),
        ),
        (
            "agent_chat",
            journaled(
                145,
                Op::AgentChat {
                    live: live(),
                    agent: s("CoreStatus"),
                    text: s("status?"),
                },
            ),
        ),
        (
            "fork_at",
            journaled(
                146,
                Op::ForkAt {
                    session: SESSION.into(),
                    entry: s("a1b2c3d4"),
                },
            ),
        ),
        (
            "navigate",
            journaled(
                147,
                Op::Navigate {
                    live: live(),
                    entry: s("a1b2c3d4"),
                },
            ),
        ),
        (
            "new_session_in",
            journaled(148, Op::NewSessionIn { dir: s(DIR) }),
        ),
        (
            "running_kill",
            journaled(
                149,
                Op::RunningKill {
                    ids: vec![s(RUNNING), s("r-fedcba9876543210")],
                },
            ),
        ),
        (
            "worktree_create opened",
            journaled(
                150,
                Op::WorktreeCreate {
                    project: PROJECT.into(),
                    branch: s("feat/x"),
                    base: Some(s("main")),
                    open: true,
                },
            ),
        ),
        (
            "worktree_create",
            journaled(
                151,
                Op::WorktreeCreate {
                    project: PROJECT.into(),
                    branch: s("feat/y"),
                    base: None,
                    open: false,
                },
            ),
        ),
        (
            "worktree_move",
            journaled(
                152,
                Op::WorktreeMove {
                    live: live(),
                    branch: s("feat/x"),
                },
            ),
        ),
        (
            "worktree_finish",
            journaled(
                153,
                Op::WorktreeFinish {
                    project: PROJECT.into(),
                },
            ),
        ),
    ]);
    for (case, id, r#where) in [
        (
            "worktree_continue in a worktree",
            154,
            ContinueWhere::Worktree,
        ),
        ("worktree_continue in main", 155, ContinueWhere::Main),
        (
            "worktree_continue switching back",
            156,
            ContinueWhere::SwitchBack,
        ),
    ] {
        out.push((
            case,
            journaled(
                id,
                Op::WorktreeContinue {
                    parked: s("k-0123456789abcdef"),
                    r#where,
                },
            ),
        ));
    }
    out.extend([
        ("update_check", view(157, Op::UpdateCheck {})),
        ("drive_backup", journaled(158, Op::DriveBackup {})),
        (
            "sync_backup",
            journaled(
                159,
                Op::SyncBackup {
                    project: PROJECT.into(),
                },
            ),
        ),
    ]);
    for (case, id, split) in [
        ("open_on_desktop", 160, Split::None),
        ("open_on_desktop split right", 161, Split::Right),
        ("open_on_desktop split down", 162, Split::Down),
    ] {
        out.push((
            case,
            journaled(
                id,
                Op::OpenOnDesktop {
                    session: SESSION.into(),
                    split,
                },
            ),
        ));
    }
    out.extend([
        (
            "close_on_desktop",
            journaled(
                163,
                Op::CloseOnDesktop {
                    session: SESSION.into(),
                },
            ),
        ),
        (
            "worktree_remove parked",
            journaled(
                164,
                Op::WorktreeRemove {
                    project: PROJECT.into(),
                    park: true,
                },
            ),
        ),
        (
            "omp_set a number",
            journaled(
                165,
                Op::OmpSet {
                    key: s("compaction.threshold"),
                    value: json!(0.8),
                },
            ),
        ),
        (
            "omp_set an object",
            journaled(
                166,
                Op::OmpSet {
                    key: s("modelRoles"),
                    value: json!({"default": "anthropic/claude-sonnet-4", "commit": ["a", 1, true, null]}),
                },
            ),
        ),
        (
            "omp_reset",
            journaled(
                167,
                Op::OmpReset {
                    key: s("compaction.threshold"),
                },
            ),
        ),
    ]);
    for (case, id, kind) in [
        ("update_run omp", 168, UpdateKind::Omp),
        ("update_run plugins", 169, UpdateKind::Plugins),
        ("update_run stable", 170, UpdateKind::Stable),
        ("update_run canary", 171, UpdateKind::Canary),
    ] {
        out.push((case, journaled(id, Op::UpdateRun { kind })));
    }
    let parts = SyncParts {
        omp: true,
        sessions: true,
        memory: false,
    };
    out.extend([
        (
            "app_update download",
            journaled(
                172,
                Op::AppUpdate {
                    step: AppUpdateStep::Download,
                },
            ),
        ),
        (
            "app_update apply",
            journaled(
                173,
                Op::AppUpdate {
                    step: AppUpdateStep::Apply,
                },
            ),
        ),
        ("drive_sign_in", journaled(174, Op::DriveSignIn {})),
        (
            "drive_delete",
            journaled(
                175,
                Op::DriveDelete {
                    ids: vec![s("1AbC"), s("1DeF")],
                },
            ),
        ),
        (
            "drive_restore_prepare",
            journaled(176, Op::DriveRestorePrepare { id: s("1AbC") }),
        ),
        (
            "drive_restore_apply",
            journaled(177, Op::DriveRestoreApply { handle: 3 }),
        ),
        (
            "drive_restore_discard",
            journaled(178, Op::DriveRestoreDiscard { handle: 3 }),
        ),
        ("sync_add", journaled(179, Op::SyncAdd { dir: s(DIR) })),
        (
            "sync_set",
            journaled(
                180,
                Op::SyncSet {
                    project: PROJECT.into(),
                    parts,
                    excluded: vec![SESSION.into()],
                },
            ),
        ),
        (
            "sync_remove",
            journaled(
                181,
                Op::SyncRemove {
                    project: PROJECT.into(),
                },
            ),
        ),
        (
            "sync_delete",
            journaled(
                182,
                Op::SyncDelete {
                    ids: vec![s("1GhI")],
                },
            ),
        ),
        (
            "sync_restore_prepare onto a project",
            journaled(
                183,
                Op::SyncRestorePrepare {
                    backup: s("1GhI"),
                    target: RestoreTarget::Project {
                        project: PROJECT.into(),
                    },
                    parts,
                },
            ),
        ),
        (
            "sync_restore_prepare onto a folder",
            journaled(
                184,
                Op::SyncRestorePrepare {
                    backup: s("1GhI"),
                    target: RestoreTarget::Dir { dir: s(DIR) },
                    parts: SyncParts::default(),
                },
            ),
        ),
        (
            "sync_restore_apply",
            journaled(
                185,
                Op::SyncRestoreApply {
                    handle: 4,
                    conflicts: vec![
                        ConflictChoice {
                            session: s("01A"),
                            choice: ConflictResolution::Keep,
                        },
                        ConflictChoice {
                            session: s("01B"),
                            choice: ConflictResolution::Overwrite,
                        },
                        ConflictChoice {
                            session: s("01C"),
                            choice: ConflictResolution::Both,
                        },
                    ],
                },
            ),
        ),
    ]);
    let subscribe = |id: u64, feed: Feed| view(id, Op::Subscribe { feed });
    out.extend([
        ("subscribe running", subscribe(186, Feed::Running {})),
        (
            "subscribe inventory",
            subscribe(
                187,
                Feed::Inventory {
                    project: None,
                    limit: 200,
                },
            ),
        ),
        (
            "subscribe inventory of a project",
            subscribe(
                188,
                Feed::Inventory {
                    project: Some(PROJECT.into()),
                    limit: 50,
                },
            ),
        ),
        (
            "subscribe context",
            subscribe(189, Feed::Context { live: live() }),
        ),
        ("subscribe usage", subscribe(190, Feed::Usage {})),
        (
            "subscribe agents",
            subscribe(
                191,
                Feed::Agents {
                    session: SESSION.into(),
                },
            ),
        ),
        (
            "subscribe worktrees",
            subscribe(
                192,
                Feed::Worktrees {
                    project: PROJECT.into(),
                },
            ),
        ),
        ("subscribe drive", subscribe(193, Feed::Drive {})),
        ("subscribe sync", subscribe(194, Feed::Sync {})),
        ("subscribe updates", subscribe(195, Feed::Updates {})),
        (
            "unsubscribe device",
            view(
                196,
                Op::Unsubscribe {
                    feed: Feed::Device {},
                },
            ),
        ),
    ]);
    out
}

pub fn drive_plan() -> RestorePlanView {
    RestorePlanView::Drive {
        name: s("omp-config-2026-10-30.zip"),
        created_unix_ms: Some(AT),
        host: s("workstation"),
        replaced: vec![s("~/.omp/agent/config.yml")],
        added: vec![s("~/.omp/agent/skills/x/SKILL.md")],
        dirs: vec![s("~/.omp/agent/skills/x")],
        removed: vec![],
        safety_display: s("~/.omp/agent/.restore-safety/2026-10-30"),
    }
}

fn project_plan() -> RestorePlanView {
    RestorePlanView::Project {
        target_display: s("~/src/app"),
        create_target: false,
        omp_exists: true,
        omp_overwritten: vec![s(".omp/AGENTS.md")],
        omp_added: vec![s(".omp/rules/x.md")],
        omp_skipped: vec![],
        memory_exists: false,
        memory_overwritten: vec![],
        memory_added: vec![s("memory/notes.md")],
        memory_skipped: vec![s("memory/huge.bin")],
        sessions: vec![
            SessionPlanView {
                id: s("01A"),
                title: s("Fix the parser"),
                modified_unix_ms: AT,
                bytes: 120_000,
                local: vec![LocalCopyView {
                    project: Some(PROJECT.into()),
                    display: Some(s("~/src/app")),
                    modified_unix_ms: AT + 1000,
                    bytes: 118_000,
                }],
                problem: None,
            },
            SessionPlanView {
                id: s("01B"),
                title: s("Broken"),
                modified_unix_ms: AT,
                bytes: 0,
                local: vec![],
                problem: Some(s("unreadable header")),
            },
        ],
    }
}

fn replies() -> Vec<(&'static str, Reply)> {
    let ok = |id: u64, r: Resp| Reply {
        id,
        result: ReplyResult::Ok(r),
    };
    let node = |id: &str, parent: Option<&str>, kind: TreeNodeKind| TreeNode {
        id: s(id),
        parent: parent.map(s),
        kind,
        subtype: None,
        at_unix_ms: Some(AT),
        text: s("row"),
        has_text: true,
        stop_reason: None,
        label: None,
        forkable: false,
        cost_before: Some(0.5),
        cost_incomplete: false,
    };
    let nodes = vec![
        TreeNode {
            forkable: true,
            cost_before: None,
            ..node("n01", None, TreeNodeKind::User)
        },
        TreeNode {
            stop_reason: Some(s("stop")),
            ..node("n02", Some("n01"), TreeNodeKind::Assistant)
        },
        node("n03", Some("n02"), TreeNodeKind::ToolResult),
        node("n04", Some("n03"), TreeNodeKind::Developer),
        node("n05", Some("n04"), TreeNodeKind::BashExecution),
        TreeNode {
            subtype: Some(s("hookMessage")),
            ..node("n06", Some("n05"), TreeNodeKind::OtherRole)
        },
        TreeNode {
            subtype: Some(s("async-result")),
            ..node("n07", Some("n06"), TreeNodeKind::CustomMessage)
        },
        node("n08", Some("n07"), TreeNodeKind::Compaction),
        node("n09", Some("n08"), TreeNodeKind::BranchSummary),
        node("n10", Some("n09"), TreeNodeKind::ModelChange),
        node("n11", Some("n10"), TreeNodeKind::ModelUsage),
        node("n12", Some("n11"), TreeNodeKind::ThinkingChange),
        TreeNode {
            label: Some(s("checkpoint")),
            ..node("n13", Some("n12"), TreeNodeKind::Label)
        },
        TreeNode {
            subtype: Some(s("plan")),
            ..node("n14", Some("n13"), TreeNodeKind::Custom)
        },
        TreeNode {
            subtype: Some(s("mode")),
            has_text: false,
            text: String::new(),
            cost_incomplete: true,
            ..node("n15", Some("n14"), TreeNodeKind::Setting)
        },
    ];
    let setting = |key: &str, kind: SettingKind, value: Option<Value>| SettingView {
        key: s(key),
        kind,
        description: s("what it does"),
        value: value.clone(),
        default: value,
        modified: false,
        secret: false,
        set: true,
    };
    let done = |id: u64, result: DoneView| {
        ok(
            id,
            Resp::Outcome {
                outcome: OutcomeView::Done { result },
            },
        )
    };
    vec![
        (
            "text",
            ok(
                101,
                Resp::Text {
                    text: s("Done."),
                    truncated: false,
                },
            ),
        ),
        (
            "download",
            ok(
                102,
                Resp::Download {
                    download: s("x-1"),
                    name: s("session.html"),
                    mime: s("text/html"),
                    size: 1_048_577,
                },
            ),
        ),
        (
            "chunk",
            ok(
                105,
                Resp::Chunk {
                    data: s("PGh0bWw+"),
                    eof: false,
                },
            ),
        ),
        (
            "last chunk",
            ok(
                105,
                Resp::Chunk {
                    data: String::new(),
                    eof: true,
                },
            ),
        ),
        (
            "tree",
            ok(
                106,
                Resp::Tree {
                    nodes,
                    active_leaf: Some(s("n15")),
                    next_cursor: Some(s("c-15")),
                    truncated: false,
                },
            ),
        ),
        (
            "dir roots",
            ok(
                110,
                Resp::Dir {
                    dir: None,
                    display: s("~"),
                    parent: None,
                    entries: vec![DirEntry {
                        name: s("src"),
                        dir: s(DIR),
                        project: None,
                        repo: false,
                    }],
                    truncated: false,
                },
            ),
        ),
        (
            "dir",
            ok(
                111,
                Resp::Dir {
                    dir: Some(s(DIR)),
                    display: s("~/src"),
                    parent: Some(s("f-fedcba9876543210")),
                    entries: vec![DirEntry {
                        name: s("app"),
                        dir: s("f-00112233445566aa"),
                        project: Some(PROJECT.into()),
                        repo: true,
                    }],
                    truncated: true,
                },
            ),
        ),
        (
            "activity",
            ok(
                112,
                Resp::Activity {
                    days: vec![
                        DayCount {
                            day: s("2026-10-29"),
                            cost: Some(1.25),
                            requests: 48,
                            tokens: 1_200_000,
                        },
                        DayCount {
                            day: s("2026-10-30"),
                            cost: None,
                            requests: 0,
                            tokens: 0,
                        },
                    ],
                },
            ),
        ),
        (
            "omp_settings",
            ok(
                114,
                Resp::OmpSettings {
                    groups: vec![SettingsGroup {
                        id: s("general"),
                        label: s("General"),
                        entries: vec![
                            setting("autoUpdate", SettingKind::Bool {}, Some(json!(true))),
                            SettingView {
                                modified: true,
                                value: Some(json!(0.8)),
                                ..setting(
                                    "compaction.threshold",
                                    SettingKind::Number {},
                                    Some(json!(0.75)),
                                )
                            },
                            setting(
                                "theme",
                                SettingKind::Enum {
                                    options: vec![s("dark"), s("light")],
                                },
                                Some(json!("dark")),
                            ),
                            SettingView {
                                secret: true,
                                value: None,
                                ..setting("apiKey", SettingKind::String {}, None)
                            },
                            setting("paths", SettingKind::Array {}, Some(json!(["a", "b"]))),
                            setting(
                                "modelRoles",
                                SettingKind::Record {},
                                Some(json!({"a": 1, "b": {"c": null}})),
                            ),
                        ],
                    }],
                    truncated: false,
                },
            ),
        ),
        (
            "drive_backups",
            ok(
                115,
                Resp::DriveBackups {
                    hosts: vec![DriveHost {
                        host: s("workstation"),
                        backups: vec![DriveBackupView {
                            id: s("1AbC"),
                            name: s("omp-config-2026-10-30.zip"),
                            size: 52_000,
                            created_unix_ms: Some(AT),
                        }],
                    }],
                    truncated: false,
                },
            ),
        ),
        (
            "sync_backups",
            ok(
                116,
                Resp::SyncBackups {
                    backups: vec![
                        SyncBackupView {
                            id: s("1GhI"),
                            name: s("app"),
                            host: s("workstation"),
                            display: s("~/src/app"),
                            size: 3_400_000,
                            created_unix_ms: Some(AT),
                            omp_bytes: Some(12_000),
                            memory_bytes: Some(0),
                            sessions: Some(14),
                            session_bytes: Some(3_388_000),
                        },
                        SyncBackupView {
                            id: s("1JkL"),
                            name: s("old"),
                            host: s("laptop"),
                            display: s("~/old"),
                            size: 10,
                            created_unix_ms: None,
                            omp_bytes: None,
                            memory_bytes: None,
                            sessions: None,
                            session_bytes: None,
                        },
                    ],
                    truncated: true,
                },
            ),
        ),
        ("upload", ok(124, Resp::Upload { upload: s("x-2") })),
        (
            "done text",
            done(
                185,
                DoneView::Text {
                    text: s("Restored 3 files."),
                },
            ),
        ),
        (
            "done killed",
            done(
                149,
                DoneView::Killed {
                    results: vec![
                        KillResult {
                            id: s(RUNNING),
                            error: None,
                        },
                        KillResult {
                            id: s("r-fedcba9876543210"),
                            error: Some(s("already gone")),
                        },
                    ],
                },
            ),
        ),
        (
            "done drive restore plan",
            done(
                176,
                DoneView::RestorePlan {
                    handle: 3,
                    plan: drive_plan(),
                    truncated: false,
                },
            ),
        ),
        (
            "done project restore plan",
            done(
                183,
                DoneView::RestorePlan {
                    handle: 4,
                    plan: project_plan(),
                    truncated: true,
                },
            ),
        ),
    ]
}

pub fn updates(phase: UpdatePhase) -> FeedValue {
    FeedValue::Updates {
        omp: OmpUpdateView {
            version: Some(s("18.14.0")),
            current: Some(s("18.14.0")),
            latest: Some(s("18.15.0")),
            error: None,
            checked_unix_ms: Some(AT),
            checking: false,
        },
        app: AppUpdateView {
            install: InstallKind::UserDir,
            apply_allowed: true,
            current: s("0.9.0"),
            latest: Some(s("0.10.0")),
            page_url: Some(s("https://github.com/o/OmpDeck/releases/tag/v0.10.0")),
            asset: Some(s("omp-deck-linux-x86_64.tar.gz")),
            checked_unix_ms: Some(AT),
            phase,
        },
    }
}

pub fn device() -> FeedValue {
    FeedValue::Device {
        name: s("Pixel"),
        permissions: Permissions {
            control: true,
            manage: true,
        },
        push: PushStateView {
            subscribed: true,
            prefs: PushPrefs::default(),
            since_unix_ms: Some(AT),
        },
        gateway_version: s("0.9.0"),
        mode: GatewayMode::Background,
    }
}

fn ui(request: &str, kind: UiKind) -> UiRequestView {
    UiRequestView {
        request: s(request),
        kind,
        title: s("Pick one"),
        message: None,
        timeout_ms: None,
        options: vec![],
        multi: false,
        initial: None,
        prefill: None,
        questions: vec![],
        approve: None,
    }
}

fn pending_ui() -> Vec<UiRequestView> {
    let option = |label: &str, description: Option<&str>| UiOption {
        label: s(label),
        description: description.map(s),
    };
    vec![
        UiRequestView {
            message: Some(s("Which branch?")),
            timeout_ms: Some(30_000),
            options: vec![option("main", None), option("dev", Some("unstable"))],
            initial: Some(1),
            ..ui("u-11", UiKind::Select)
        },
        UiRequestView {
            options: vec![option("a", None), option("b", None), option("c", None)],
            multi: true,
            ..ui("u-12", UiKind::Select)
        },
        UiRequestView {
            message: Some(s("Delete 3 files?")),
            ..ui("u-13", UiKind::Confirm)
        },
        UiRequestView {
            prefill: Some(s("feat/")),
            ..ui("u-14", UiKind::Input)
        },
        UiRequestView {
            prefill: Some(s("line 1\nline 2")),
            ..ui("u-15", UiKind::Editor)
        },
        UiRequestView {
            questions: vec![
                UiQuestion {
                    id: s("scope"),
                    question: s("Which scope?"),
                    header: Some(s("Scope")),
                    options: vec![option("crate", None), option("workspace", None)],
                    multi: false,
                    recommended: Some(0),
                },
                UiQuestion {
                    id: s("extras"),
                    question: s("Extras?"),
                    header: None,
                    options: vec![option("docs", Some("update docs"))],
                    multi: true,
                    recommended: None,
                },
            ],
            ..ui("u-16", UiKind::Ask)
        },
        UiRequestView {
            approve: Some(ApproveView {
                full: s("bash: rm -rf target"),
                compact: s("rm -rf target"),
                session_label: Some(s("Allow for this session")),
                grants: vec![ApproveGrant {
                    scope: s("folder"),
                    label: s("Allow in ~/src/app"),
                    always_label: s("Always allow in ~/src/app"),
                }],
            }),
            ..ui("u-17", UiKind::Approve)
        },
        UiRequestView {
            approve: Some(ApproveView {
                full: s("write a.txt"),
                compact: s("write"),
                session_label: None,
                grants: vec![],
            }),
            ..ui("u-18", UiKind::Approve)
        },
    ]
}

fn feeds() -> Vec<(&'static str, Event)> {
    let feed = |feed: Feed, value: FeedValue| Event::Feed { feed, value };
    let image = || ImageRef {
        blob: s("0123456789abcdef"),
        mime: s("image/png"),
        bytes: 48_213,
    };
    let running = |id: &str, backend: MuxBackend| RunningView {
        id: s(id),
        sessions: vec![],
        project: None,
        display: None,
        title: None,
        backend,
        started_unix_ms: None,
        pid: None,
        memory_bytes: None,
        processes: None,
        live: None,
    };
    let agent = |id: &str, status: AgentStatus| AgentView {
        id: s(id),
        parent: None,
        depth: 0,
        label: s("Map the scanner"),
        agent: Some(s("task")),
        status,
        model: None,
        thinking: None,
        error: None,
        current: None,
        created_unix_ms: Some(AT),
        last_activity_unix_ms: None,
        duration_ms: None,
        requests: 0,
        tool_calls: 0,
        tokens: 0,
        cost: None,
        context_tokens: None,
        has_output: false,
        live: false,
    };
    let drive = |interval: SyncInterval| FeedValue::Drive {
        available: true,
        signed_in: false,
        account: None,
        last_backup_unix_ms: None,
        interval,
        busy: None,
        error: None,
    };
    let sync = |interval: SyncInterval| FeedValue::Sync {
        rows: vec![],
        interval,
        busy: None,
        truncated: false,
    };
    let app = |install: InstallKind, phase: UpdatePhase| match updates(phase) {
        FeedValue::Updates { omp, app } => FeedValue::Updates {
            omp,
            app: AppUpdateView {
                install,
                apply_allowed: false,
                ..app
            },
        },
        other => other,
    };
    let mode = |mode: GatewayMode| match device() {
        FeedValue::Device {
            name,
            permissions,
            push,
            gateway_version,
            ..
        } => FeedValue::Device {
            name,
            permissions,
            push,
            gateway_version,
            mode,
        },
        other => other,
    };
    let live_session = LiveSession {
        live: live(),
        kind: LiveKind::Session,
        label: s("Fix the parser"),
        title: Some(s("Fix the parser")),
        session: Some(SESSION.into()),
        project: Some(PROJECT.into()),
        display: s("~/src/app"),
        activity: Activity::Attention,
        status: Status::Attention {},
        exit_code: None,
        busy: false,
        model: None,
        pending_switch: None,
        live_control: true,
        desktop_controlled: false,
        views: 0,
        requests: vec![],
        pending_ui: pending_ui(),
    };
    let mut out = vec![
        (
            "feed live with pending ui",
            feed(
                Feed::Live {},
                FeedValue::Live {
                    sessions: vec![live_session],
                    truncated: false,
                },
            ),
        ),
        (
            "feed sessions with a stub",
            feed(
                Feed::Sessions {
                    project: PROJECT.into(),
                    limit: 2,
                },
                FeedValue::Sessions {
                    project: PROJECT.into(),
                    total: 2,
                    sessions: vec![
                        SessionSummary {
                            id: s("01HZX3J6V0Q9"),
                            title: None,
                            first_prompt: None,
                            modified_unix_ms: AT,
                            stub: true,
                        },
                        SessionSummary {
                            id: SESSION.into(),
                            title: Some(s("Fix the parser")),
                            first_prompt: Some(s("Fix it")),
                            modified_unix_ms: AT,
                            stub: false,
                        },
                    ],
                    truncated: false,
                },
            ),
        ),
        (
            "feed transcript with images, args and details",
            feed(
                Feed::Transcript {
                    session: SESSION.into(),
                    agent: None,
                    tail: 50,
                },
                FeedValue::Transcript {
                    start: 4,
                    reset: false,
                    items: vec![
                        Item::User {
                            text: s("see"),
                            from_agent: false,
                            at_unix_ms: Some(AT),
                            images: vec![image()],
                        },
                        Item::ToolCall {
                            call_id: s("c2"),
                            name: s("edit"),
                            intent: None,
                            summary: s("src/main.rs"),
                            args: s("{\"path\":\"src/main.rs\"}"),
                            args_value: Some(
                                json!({"path": "src/main.rs", "edits": [{"old": "a", "new": "b"}], "dry": false, "n": 2}),
                            ),
                        },
                        Item::ToolResult {
                            call_id: s("c2"),
                            name: s("edit"),
                            text: s("ok"),
                            is_error: false,
                            truncated: false,
                            details: Some(json!({"diff": "-a\n+b", "firstChangedLine": 3})),
                            images: vec![],
                        },
                        Item::ToolResult {
                            call_id: s("c3"),
                            name: s("screenshot"),
                            text: String::new(),
                            is_error: false,
                            truncated: false,
                            details: None,
                            images: vec![ImageRef {
                                blob: s("fedcba9876543210"),
                                mime: s("image/jpeg"),
                                bytes: 0,
                            }],
                        },
                    ],
                },
            ),
        ),
        (
            "feed running",
            feed(
                Feed::Running {},
                FeedValue::Running {
                    sessions: vec![
                        RunningView {
                            sessions: vec![SESSION.into()],
                            project: Some(PROJECT.into()),
                            display: Some(s("~/src/app")),
                            title: Some(s("Fix the parser")),
                            started_unix_ms: Some(AT),
                            pid: Some(4242),
                            memory_bytes: Some(512_000_000),
                            processes: Some(7),
                            live: Some(live()),
                            ..running(RUNNING, MuxBackend::Tmux)
                        },
                        running("r-fedcba9876543210", MuxBackend::Host),
                    ],
                    server_bytes: Some(9_000_000),
                    truncated: false,
                },
            ),
        ),
        (
            "feed inventory",
            feed(
                Feed::Inventory {
                    project: None,
                    limit: 200,
                },
                FeedValue::Inventory {
                    project: None,
                    total: 230,
                    entries: vec![
                        InventoryView {
                            id: SESSION.into(),
                            project: PROJECT.into(),
                            display: s("~/src/app"),
                            title: Some(s("Fix the parser")),
                            first_prompt: Some(s("Fix it")),
                            stub: false,
                            created_unix_ms: Some(AT - 60_000),
                            modified_unix_ms: AT,
                            jsonl_bytes: 1_200_000,
                            artifact_bytes: 300_000,
                            artifact_files: 4,
                            subagents: 3,
                            parent: None,
                            cwd_exists: true,
                            running: true,
                            stats: Some(InventoryStats {
                                user_messages: 12,
                                assistant_turns: 40,
                                input_tokens: 1_000_000,
                                output_tokens: 50_000,
                                cache_read_tokens: 900_000,
                                cache_write_tokens: 80_000,
                                cost: Some(3.5),
                                subagent_cost: Some(0.75),
                                model: Some(s("anthropic/claude-sonnet-4")),
                                compactions: 1,
                                last_activity_unix_ms: Some(AT),
                            }),
                        },
                        InventoryView {
                            id: s("01HZX3J6V0Q9"),
                            project: PROJECT.into(),
                            display: s("~/gone"),
                            title: None,
                            first_prompt: None,
                            stub: true,
                            created_unix_ms: None,
                            modified_unix_ms: AT,
                            jsonl_bytes: 180,
                            artifact_bytes: 0,
                            artifact_files: 0,
                            subagents: 0,
                            parent: Some(SESSION.into()),
                            cwd_exists: false,
                            running: false,
                            stats: None,
                        },
                    ],
                    truncated: true,
                },
            ),
        ),
        (
            "feed context",
            feed(
                Feed::Context { live: live() },
                FeedValue::Context {
                    live: live(),
                    context: Some(ContextView {
                        model: Some(s("anthropic/claude-sonnet-4")),
                        context_window: 200_000,
                        used_tokens: 42_000,
                        anchored: true,
                        categories: vec![
                            ContextCategory {
                                id: s("system"),
                                label: s("System prompt"),
                                tokens: 8_000,
                            },
                            ContextCategory {
                                id: s("messages"),
                                label: s("Messages"),
                                tokens: 30_000,
                            },
                        ],
                        auto_compact_buffer: 20_000,
                        free_tokens: 138_000,
                        threshold_tokens: Some(160_000),
                        snapcompact_saved: None,
                        tools: vec![ToolTokens {
                            name: s("bash"),
                            tokens: 1_200,
                        }],
                        skills: 4,
                        computed_unix_ms: AT,
                    }),
                },
            ),
        ),
        (
            "feed context unknown",
            feed(
                Feed::Context { live: live() },
                FeedValue::Context {
                    live: live(),
                    context: None,
                },
            ),
        ),
        (
            "feed usage",
            feed(
                Feed::Usage {},
                FeedValue::Usage {
                    usage: Some(UsageView {
                        reports: vec![UsageReportView {
                            provider: s("anthropic"),
                            account: Some(s("me@example.com")),
                            plan: Some(s("max")),
                            fetched_unix_ms: Some(AT),
                            limits: vec![
                                UsageLimitView {
                                    id: s("five_hour"),
                                    label: s("5-hour"),
                                    window: Some(s("5h")),
                                    resets_unix_ms: Some(AT + 3_600_000),
                                    used: Some(25.0),
                                    limit: Some(100.0),
                                    remaining: Some(75.0),
                                    used_fraction: Some(0.25),
                                    remaining_fraction: Some(0.75),
                                    unit: s("percent"),
                                    status: s("ok"),
                                },
                                UsageLimitView {
                                    id: s("weekly"),
                                    label: s("Weekly"),
                                    window: None,
                                    resets_unix_ms: None,
                                    used: None,
                                    limit: None,
                                    remaining: None,
                                    used_fraction: None,
                                    remaining_fraction: None,
                                    unit: s("unknown"),
                                    status: s("unknown"),
                                },
                            ],
                            notes: vec![s("estimated")],
                        }],
                        tightest_remaining: Some(0.75),
                        unavailable: vec![s("openai")],
                    }),
                    fetched_unix_ms: Some(AT),
                    error: None,
                    fetching: false,
                    truncated: false,
                },
            ),
        ),
        (
            "feed usage failed",
            feed(
                Feed::Usage {},
                FeedValue::Usage {
                    usage: None,
                    fetched_unix_ms: None,
                    error: Some(s("offline")),
                    fetching: true,
                    truncated: false,
                },
            ),
        ),
        (
            "feed agents",
            feed(
                Feed::Agents {
                    session: SESSION.into(),
                },
                FeedValue::Agents {
                    session: SESSION.into(),
                    agents: vec![
                        AgentView {
                            model: Some(s("anthropic/claude-sonnet-4")),
                            thinking: Some(s("high")),
                            current: Some(s("bash")),
                            last_activity_unix_ms: Some(AT),
                            duration_ms: Some(65_000),
                            requests: 9,
                            tool_calls: 21,
                            tokens: 120_000,
                            cost: Some(0.5),
                            context_tokens: Some(30_000),
                            has_output: true,
                            live: true,
                            ..agent("CoreStatus", AgentStatus::Running)
                        },
                        AgentView {
                            parent: Some(s("CoreStatus")),
                            depth: 1,
                            ..agent("CoreStatus.Scan", AgentStatus::Done)
                        },
                        AgentView {
                            error: Some(s("overloaded")),
                            ..agent("Fails", AgentStatus::Failed)
                        },
                        agent("Aborts", AgentStatus::Aborted),
                        agent("Interrupts", AgentStatus::Interrupted),
                        agent("Parks", AgentStatus::Parked),
                    ],
                    truncated: false,
                },
            ),
        ),
        (
            "feed worktrees",
            feed(
                Feed::Worktrees {
                    project: PROJECT.into(),
                },
                FeedValue::Worktrees {
                    project: PROJECT.into(),
                    worktree: Some(WorktreeView {
                        linked: true,
                        branch: Some(s("feat/x")),
                        base: Some(s("main")),
                        primary: Some(s("p-fedcba9876543210")),
                        primary_display: s("~/src/app"),
                        branches: vec![s("main"), s("feat/x"), s("origin/main")],
                        has_commits: true,
                    }),
                    parked: vec![ParkedView {
                        id: s("k-0123456789abcdef"),
                        branch: s("feat/old"),
                        base: s("main"),
                        sessions: 2,
                        parked_unix_ms: AT,
                        exists: true,
                        merged: false,
                        checked_out: false,
                    }],
                    truncated: false,
                },
            ),
        ),
        (
            "feed worktrees outside git",
            feed(
                Feed::Worktrees {
                    project: PROJECT.into(),
                },
                FeedValue::Worktrees {
                    project: PROJECT.into(),
                    worktree: None,
                    parked: vec![],
                    truncated: false,
                },
            ),
        ),
        (
            "feed drive",
            feed(
                Feed::Drive {},
                FeedValue::Drive {
                    available: true,
                    signed_in: true,
                    account: Some(s("me@example.com")),
                    last_backup_unix_ms: Some(AT),
                    interval: SyncInterval::Daily,
                    busy: Some(s("Backing up")),
                    error: Some(s("quota")),
                },
            ),
        ),
        (
            "feed sync",
            feed(
                Feed::Sync {},
                FeedValue::Sync {
                    rows: vec![SyncRowView {
                        project: PROJECT.into(),
                        display: s("~/src/app"),
                        parts: SyncParts {
                            omp: true,
                            sessions: false,
                            memory: true,
                        },
                        excluded: vec![SESSION.into()],
                        last_backup_unix_ms: None,
                    }],
                    interval: SyncInterval::Off,
                    busy: None,
                    truncated: false,
                },
            ),
        ),
    ];
    for (case, interval) in [
        ("feed drive every ten minutes", SyncInterval::TenMinutes),
        (
            "feed drive every thirty minutes",
            SyncInterval::ThirtyMinutes,
        ),
    ] {
        out.push((case, feed(Feed::Drive {}, drive(interval))));
    }
    for (case, interval) in [
        ("feed sync hourly", SyncInterval::Hourly),
        ("feed sync every six hours", SyncInterval::SixHours),
    ] {
        out.push((case, feed(Feed::Sync {}, sync(interval))));
    }
    out.push((
        "feed updates",
        feed(
            Feed::Updates {},
            updates(UpdatePhase::Downloading {
                done: 1_000,
                total: Some(9_000),
            }),
        ),
    ));
    for (case, install, phase) in [
        (
            "feed updates idle",
            InstallKind::AppImage,
            UpdatePhase::Idle {},
        ),
        (
            "feed updates checking",
            InstallKind::Deb,
            UpdatePhase::Checking {},
        ),
        (
            "feed updates applied",
            InstallKind::Rpm,
            UpdatePhase::Applied {},
        ),
        (
            "feed updates needs manual",
            InstallKind::WindowsDir,
            UpdatePhase::NeedsManual {},
        ),
        (
            "feed updates failed",
            InstallKind::Dev,
            UpdatePhase::Failed {
                message: s("checksum mismatch"),
            },
        ),
        (
            "feed updates unsupported",
            InstallKind::Unsupported,
            UpdatePhase::Downloading {
                done: 0,
                total: None,
            },
        ),
    ] {
        out.push((case, feed(Feed::Updates {}, app(install, phase))));
    }
    out.push(("feed device", feed(Feed::Device {}, device())));
    for (case, m) in [
        ("feed device while open", GatewayMode::WhileOpen),
        ("feed device off", GatewayMode::Off),
    ] {
        out.push((case, feed(Feed::Device {}, mode(m))));
    }
    out
}

fn events() -> Vec<(&'static str, Event)> {
    let mut out = feeds();
    out.push((
        "outcome restore plan",
        Event::Outcome {
            seq: 176,
            outcome: OutcomeView::Done {
                result: DoneView::RestorePlan {
                    handle: 3,
                    plan: drive_plan(),
                    truncated: false,
                },
            },
        },
    ));
    out
}

fn welcomes() -> Vec<(&'static str, Welcome)> {
    vec![(
        "welcome with s4 and a push key",
        Welcome {
            protocol: PROTOCOL,
            gateway_version: s("0.10.0"),
            epoch: s("00112233445566778899aabbccddeeff"),
            device: s("d-0123456789abcdef"),
            name: s("Pixel"),
            permissions: Permissions {
                control: true,
                manage: true,
            },
            next_seq: 186,
            limits: Limits::default(),
            features: vec![FEATURE_S4.into()],
            push_key: s(
                "BDd3_hVL9fZi9Ybo2UUzA284WG5FZR30_95YeZJsiApwXKpNcF1rRPF3foIiBHXRdJI2Qhumhf6_LFTeZaNndIo",
            ),
        },
    )]
}

/// `(case, json, expected)`: slice-4 request bodies a gateway refuses.
fn rejected_requests() -> Vec<(&'static str, &'static str, Value)> {
    vec![
        (
            "unknown compose mode",
            r#"{"id":201,"seq":201,"op":{"op":"compose","live":{"key":"k","generation":1},"text":"x","attachments":[],"mode":"shout"}}"#,
            json!({"error": "unsupported", "id": 201}),
        ),
        (
            "unknown tree filter",
            r#"{"id":202,"op":{"op":"tree","session":"s","filter":"recent","cursor":null,"limit":10}}"#,
            json!({"error": "unsupported", "id": 202}),
        ),
        (
            "unknown review side",
            r#"{"id":203,"seq":203,"op":{"op":"review","live":{"key":"k","generation":1},"comments":[{"file":"a","side":"left","start":1,"end":1,"text":"x"}],"general":null,"send":true}}"#,
            json!({"error": "unsupported", "id": 203}),
        ),
        (
            "unknown ui answer type",
            r#"{"id":204,"seq":204,"op":{"op":"answer_ui","live":{"key":"k","generation":1},"request":"u-1","value":{"type":"rating","stars":5}}}"#,
            json!({"error": "unsupported", "id": 204}),
        ),
        (
            "unknown approve decision",
            r#"{"id":205,"seq":205,"op":{"op":"answer_ui","live":{"key":"k","generation":1},"request":"u-1","value":{"type":"approve","decision":{"type":"forever"}}}}"#,
            json!({"error": "unsupported", "id": 205}),
        ),
        (
            "unknown restore target",
            r#"{"id":206,"seq":206,"op":{"op":"sync_restore_prepare","backup":"b","target":{"type":"path","path":"/tmp"},"parts":{"omp":true,"sessions":true,"memory":true}}}"#,
            json!({"error": "unsupported", "id": 206}),
        ),
        (
            "unknown conflict choice",
            r#"{"id":207,"seq":207,"op":{"op":"sync_restore_apply","handle":1,"conflicts":[{"session":"s","choice":"merge"}]}}"#,
            json!({"error": "unsupported", "id": 207}),
        ),
        (
            "unknown slice-4 feed",
            r#"{"id":208,"op":{"op":"subscribe","feed":{"feed":"weather"}}}"#,
            json!({"error": "unsupported", "id": 208}),
        ),
        (
            "unknown field in push prefs",
            r#"{"id":209,"op":{"op":"push_prefs","prefs":{"turn_finished":true,"attention":true,"ended":true,"show_titles":false,"sound":true}}}"#,
            json!({"error": "bad_request", "id": 209}),
        ),
        (
            "missing sync part",
            r#"{"id":210,"seq":210,"op":{"op":"sync_set","project":"p","parts":{"omp":true,"sessions":true},"excluded":[]}}"#,
            json!({"error": "bad_request", "id": 210}),
        ),
        (
            "negative upload offset",
            r#"{"id":211,"op":{"op":"upload_chunk","upload":"x-1","offset":-1,"data":""}}"#,
            json!({"error": "bad_request", "id": 211}),
        ),
        (
            "unknown field in an ui answer",
            r#"{"id":212,"seq":212,"op":{"op":"answer_ui","live":{"key":"k","generation":1},"request":"u-1","value":{"type":"confirm","yes":true,"why":"x"}}}"#,
            json!({"error": "bad_request", "id": 212}),
        ),
    ]
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
    out
}

fn build() -> Value {
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
        "description": "Slice-4 additions to the device application protocol (deck_remote::api, docs/notes/mobile-pwa.md \"Wire additions\") shared with pwa/src/remote: sent only when both sides announce the features listed here. messages: the canonical JSON text of every new op, feed, value, reply, result and enum value and of the additive fields of R5 types (decode equals the value, encode equals the text; None is null except the additive fields, which are omitted when empty); rejected_requests: REQUEST bodies answered unsupported/bad_request (by id). Records and framing are unchanged (api-v1.json). Generated by crates/deck-remote/tests/api_s4/mod.rs.",
        "protocol": PROTOCOL,
        "features": [FEATURE_S4],
        "messages": messages(),
        "rejected_requests": requests,
    })
}

#[test]
fn s4_vectors_match() {
    let built = build();
    let text = serde_json::to_string_pretty(&built).unwrap() + "\n";
    if std::env::var_os("OMPDECK_WRITE_VECTORS").is_some() {
        std::fs::write(PATH, &text).unwrap();
    }
    let file = std::fs::read_to_string(PATH).expect("api-s4.json (OMPDECK_WRITE_VECTORS=1)");
    assert!(file == text, "api-s4.json drifted: OMPDECK_WRITE_VECTORS=1");
}

#[test]
fn s4_messages_decode_to_their_value_and_encode_to_their_text() {
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
    for (case, r) in requests() {
        let text = serde_json::to_vec(&r).unwrap();
        assert!(Request::parse(&text).is_ok(), "{case}");
        assert!(r.check_seq().is_ok(), "{case}");
        assert!(r.op.check().is_ok(), "{case}");
    }
}

#[test]
fn s4_rejected_requests_are_classified() {
    for (case, text, expect) in rejected_requests() {
        let got = Request::parse(text.as_bytes()).unwrap_err();
        assert_eq!(request_error_name(&got), expect["error"], "{case}");
        assert_eq!(request_error_id(&got), expect["id"].as_u64(), "{case}");
    }
}

/// An R5 reader ignores nothing it knows: the additive fields are omitted when empty, and a
/// message without them decodes to the empty value.
#[test]
fn additive_fields_are_omitted_when_empty_and_default_when_absent() {
    let summary: SessionSummary =
        serde_json::from_str(r#"{"id":"a","title":null,"first_prompt":null,"modified_unix_ms":1}"#)
            .unwrap();
    assert!(!summary.stub);
    assert!(!json_text(&summary).contains("stub"));
    let item: Item = serde_json::from_str(
        r#"{"type":"tool_call","call_id":"c","name":"n","intent":null,"summary":"","args":""}"#,
    )
    .unwrap();
    assert_eq!(
        item,
        Item::ToolCall {
            call_id: s("c"),
            name: s("n"),
            intent: None,
            summary: String::new(),
            args: String::new(),
            args_value: None,
        }
    );
    let welcome = &welcomes()[0].1;
    let mut value = serde_json::to_value(welcome).unwrap();
    value.as_object_mut().unwrap().remove("push_key");
    let decoded: Welcome = serde_json::from_value(value).unwrap();
    assert!(decoded.push_key.is_empty());
    assert!(!json_text(&decoded).contains("push_key"));
}

/// Out-of-range parameters parse but fail [`Op::check`] (answered `bad_request`).
#[test]
fn s4_parameter_bounds() {
    let tree = |limit| Op::Tree {
        session: SESSION.into(),
        filter: TreeFilter::Default,
        cursor: None,
        limit,
    };
    assert!(tree(0).check().is_err());
    assert!(tree(MAX_TREE_LIMIT + 1).check().is_err());
    assert!(Op::UsageActivity { days: 0 }.check().is_err());
    assert!(
        Op::UsageActivity {
            days: MAX_ACTIVITY_DAYS + 1
        }
        .check()
        .is_err()
    );
    let chunk = Op::UploadChunk {
        upload: s("x-1"),
        offset: 0,
        data: "A".repeat(MAX_TRANSFER_CHUNK_B64 + 4),
    };
    assert!(chunk.check().is_err());
    let inventory = Op::Subscribe {
        feed: Feed::Inventory {
            project: None,
            limit: MAX_INVENTORY_LIMIT + 1,
        },
    };
    assert!(inventory.check().is_err());
}
