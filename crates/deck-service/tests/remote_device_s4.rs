//! Slice 4 of the device protocol end to end (a device announcing `s4`), like
//! `remote_device.rs`: the welcome's features and push key, the folder browser (roots, a
//! listing, `new_session_in`) without absolute paths, an upload composed into omp's editor, an
//! export pulled by download chunks, the `running` feed and `running_kill`, a Manage op refused
//! without the permission, `worktree_create` on the gateway, slice-4 ops answered `unsupported`
//! to a device that did not announce `s4`, and `unpair` answered before the device is revoked.
//!
//! Private HOME/XDG tree, own `omp-deck-test-*` namespace; skipped when omp is not installed.

mod support;

use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use deck_remote::api::{
    self, ApiError, DoneView, Event, Feed, FeedValue, Op, OutcomeView, Record, ReplyResult, Resp,
};
use deck_service::gateway::client::Link;
use deck_service::gateway::protocol::{Mutation, Permissions, RelayLink, SecretText};
use support::device::{
    Device, OMP_CONFIG, Relay, SLICE, STEP, Sandbox, accepted, event, pair, run, wait_for,
    wait_mobile,
};

// ---- the test -----------------------------------------------------------------------------------

/// `git` in `dir` with a fixed identity; panics on failure.
fn git(dir: &std::path::Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .expect("git");
    assert!(out.status.success(), "git {args:?}: {out:?}");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Every reply and event `d` received so far, as JSON, must be free of `root`.
fn no_paths(d: &Device, root: &str) {
    for r in &d.records {
        if let Record::Event(b) | Record::Reply(b) = r {
            let s = String::from_utf8_lossy(b);
            assert!(!s.contains(root), "absolute path sent to a device: {s}");
        }
    }
}

fn no_path_in<T: serde::Serialize + std::fmt::Debug>(v: &T, root: &str) {
    let json = serde_json::to_string(v).expect("json");
    assert!(
        !json.contains(root),
        "absolute path sent to a device: {json}"
    );
}

fn done(o: OutcomeView) -> DoneView {
    match o {
        OutcomeView::Done { result } => result,
        other => panic!("not done: {other:?}"),
    }
}

#[test]
fn slice4_device_ops_through_the_relay() {
    if which(if cfg!(windows) { "omp.exe" } else { "omp" }).is_none() {
        eprintln!("skipped: omp is not installed");
        return;
    }
    let sb = Sandbox::new("rd4", OMP_CONFIG);
    sb.report_failures();
    sb.seed_session("seed-device-4");
    let git_ok = which(if cfg!(windows) { "git.exe" } else { "git" }).is_some();
    if git_ok {
        git(&sb.proj(), &["init", "-q", "-b", "main"]);
        std::fs::write(sb.proj().join("README"), "x\n").expect("readme");
        git(&sb.proj(), &["add", "README"]);
        git(&sb.proj(), &["commit", "-q", "-m", "init"]);
    }
    let relay = Relay::start(sb.root.join("relay"));
    let root = sb.root.to_string_lossy().into_owned();

    let (phone, plain) = {
        let client = sb.client();
        wait_for("the gateway link", Duration::from_secs(30), || {
            matches!(client.link(), Link::Up(_)).then_some(())
        });
        run(
            &client,
            Mutation::RelaySet {
                url: relay.url(),
                room_key: SecretText::new(relay.key.encode()),
            },
        )
        .expect("relay set");
        wait_mobile(&client, "the relay link", |m| m.link == RelayLink::Up);
        let control = Permissions {
            control: true,
            manage: false,
        };
        (
            pair(&client, &relay, "Phone", control),
            pair(&client, &relay, "Plain", control),
        )
    };
    let project = api::project_id(sb.proj().to_string_lossy().as_bytes());

    // -- a device without `s4`: slice-4 ops and feeds are `unsupported` --
    let mut old = Device::connect(&relay, &plain, "Plain", false);
    let r = old.call(&mut [], None, Op::Browse { dir: None });
    assert!(
        matches!(r, ReplyResult::Err(ApiError::Unsupported {})),
        "{r:?}"
    );
    let r = old.call(
        &mut [],
        None,
        Op::Subscribe {
            feed: Feed::Running {},
        },
    );
    assert!(
        matches!(r, ReplyResult::Err(ApiError::Unsupported {})),
        "{r:?}"
    );
    drop(old);

    let mut a = Device::connect(&relay, &phone, "Phone", true);
    assert_eq!(a.welcome.features, vec![api::FEATURE_S4.to_owned()]);
    assert!(!a.welcome.push_key.is_empty(), "no push key in the welcome");
    let mut seq = a.welcome.next_seq;

    // -- Manage refused without the permission --
    let r = a.call(&mut [], Some(seq), Op::DriveSignIn {});
    seq += 1;
    assert!(
        matches!(
            r,
            ReplyResult::Err(ApiError::NotAllowed {
                need: api::Class::Manage
            })
        ),
        "{r:?}"
    );

    // -- the folder browser: roots, the home listing, a new session by dir token --
    let roots = match a.call(&mut [], None, Op::Browse { dir: None }) {
        ReplyResult::Ok(Resp::Dir {
            dir: None, entries, ..
        }) => entries,
        other => panic!("browse roots: {other:?}"),
    };
    no_path_in(&roots, &root);
    let home = roots
        .iter()
        .find(|e| e.name == "~")
        .expect("home is a root");
    // Symlinked folders count where they lead: out of every root is never offered, inside one
    // is kept; `flip` leads inside now and out of the roots once its token was handed out.
    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        let outside = sb.root.join("outside");
        std::fs::create_dir_all(outside.join("secret")).expect("outside dir");
        let home_dir = sb.root.join("home");
        symlink(&outside, home_dir.join("escape")).expect("escape link");
        symlink(sb.proj(), home_dir.join("alias")).expect("alias link");
        symlink(sb.proj(), home_dir.join("flip")).expect("flip link");
    }
    let listing = match a.call(
        &mut [],
        None,
        Op::Browse {
            dir: Some(home.dir.clone()),
        },
    ) {
        ReplyResult::Ok(Resp::Dir {
            entries, display, ..
        }) => {
            assert_eq!(display, "~");
            entries
        }
        other => panic!("browse home: {other:?}"),
    };
    no_path_in(&listing, &root);
    let proj = listing
        .iter()
        .find(|e| e.name == "proj")
        .expect("proj listed");
    assert_eq!(proj.project.as_deref(), Some(project.as_str()));
    assert_eq!(proj.repo, git_ok);
    #[cfg(unix)]
    {
        let names: Vec<&str> = listing.iter().map(|e| e.name.as_str()).collect();
        assert!(
            !names.contains(&"escape"),
            "a link out of the roots: {names:?}"
        );
        assert!(names.contains(&"alias"), "a link inside a root: {names:?}");
        let flip = listing
            .iter()
            .find(|e| e.name == "flip")
            .expect("flip listed")
            .dir
            .clone();
        let link = sb.root.join("home/flip");
        std::fs::remove_file(&link).expect("unlink flip");
        std::os::unix::fs::symlink(sb.root.join("outside"), &link).expect("flip out");
        let r = a.call(
            &mut [],
            None,
            Op::Browse {
                dir: Some(flip.clone()),
            },
        );
        assert!(
            matches!(r, ReplyResult::Err(ApiError::NotFound { .. })),
            "a token leading out of the roots: {r:?}"
        );
        // Nor does a session start there.
        let r = a.call(&mut [], Some(seq), Op::NewSessionIn { dir: flip });
        seq += 1;
        assert!(
            matches!(r, ReplyResult::Err(ApiError::NotFound { .. })),
            "a session in a folder out of the roots: {r:?}"
        );
    }
    let launch = seq;
    seq += 1;
    assert!(!accepted(a.call(
        &mut [],
        Some(launch),
        Op::NewSessionIn {
            dir: proj.dir.clone()
        }
    )));
    let live = match done(a.outcome(&mut [], launch, Duration::from_secs(60))) {
        DoneView::Session { live } => live,
        other => panic!("new_session_in: {other:?}"),
    };
    let r = a.call(
        &mut [],
        None,
        Op::Subscribe {
            feed: Feed::Live {},
        },
    );
    assert!(matches!(r, ReplyResult::Ok(Resp::Unit {})), "{r:?}");
    a.live(
        &mut [],
        "an idle live-control session",
        Duration::from_secs(90),
        |s| {
            s.iter()
                .any(|s| s.live == live && s.live_control && !s.busy)
        },
    );

    // -- an upload composed into omp's editor --
    let upload = match a.call(
        &mut [],
        None,
        Op::UploadBegin {
            name: "note.txt".into(),
            mime: "text/plain".into(),
            size: 5,
        },
    ) {
        ReplyResult::Ok(Resp::Upload { upload }) => upload,
        other => panic!("upload_begin: {other:?}"),
    };
    let r = a.call(
        &mut [],
        None,
        Op::UploadChunk {
            upload: upload.clone(),
            offset: 0,
            data: "aGVsbG8=".into(),
        },
    );
    assert!(matches!(r, ReplyResult::Ok(Resp::Unit {})), "{r:?}");
    let compose = seq;
    seq += 1;
    assert!(!accepted(a.call(
        &mut [],
        Some(compose),
        Op::Compose {
            live: live.clone(),
            text: "device-s4-compose".into(),
            attachments: vec![upload],
            mode: api::ComposeMode::Insert,
        },
    )));
    match a.outcome(&mut [], compose, Duration::from_secs(60)) {
        OutcomeView::Done { .. } => {}
        other => panic!("compose: {other:?}"),
    }

    // -- an export pulled by download chunks --
    let (download, size) = match a.call(
        &mut [],
        None,
        Op::Export {
            session: "seed-device-4".into(),
        },
    ) {
        ReplyResult::Ok(Resp::Download {
            download,
            size,
            mime,
            ..
        }) => {
            assert_eq!(mime, "text/html");
            (download, size)
        }
        other => panic!("export: {other:?}"),
    };
    let mut offset = 0u64;
    loop {
        match a.call(
            &mut [],
            None,
            Op::Download {
                download: download.clone(),
                offset,
            },
        ) {
            ReplyResult::Ok(Resp::Chunk { data, eof }) => {
                let pad = data.bytes().rev().take_while(|b| *b == b'=').count();
                offset += (data.len() / 4 * 3 - pad) as u64;
                if eof {
                    break;
                }
            }
            other => panic!("download: {other:?}"),
        }
    }
    assert_eq!(offset, size, "every byte of the export was downloaded");

    // -- worktree_create on the gateway --
    if git_ok {
        let create = seq;
        seq += 1;
        assert!(!accepted(a.call(
            &mut [],
            Some(create),
            Op::WorktreeCreate {
                project: project.clone(),
                branch: "device-wt".into(),
                base: None,
                open: false,
            },
        )));
        match done(a.outcome(&mut [], create, Duration::from_secs(60))) {
            DoneView::Unit {} => {}
            other => panic!("worktree_create: {other:?}"),
        }
        let list = git(&sb.proj(), &["worktree", "list", "--porcelain"]);
        assert!(list.contains("refs/heads/device-wt"), "{list}");
    }

    // -- the running feed and running_kill --
    let r = a.call(
        &mut [],
        None,
        Op::Subscribe {
            feed: Feed::Running {},
        },
    );
    assert!(matches!(r, ReplyResult::Ok(Resp::Unit {})), "{r:?}");
    let running = a.take(
        &mut [],
        "the running session",
        Duration::from_secs(60),
        |r| match event(r)? {
            Event::Feed {
                value: FeedValue::Running { sessions, .. },
                ..
            } => sessions
                .into_iter()
                .find(|s| s.live.as_ref() == Some(&live)),
            _ => None,
        },
    );
    no_path_in(&running, &root);
    assert!(running.id.starts_with("r-"), "{}", running.id);
    let kill = seq;
    seq += 1;
    assert!(!accepted(a.call(
        &mut [],
        Some(kill),
        Op::RunningKill {
            ids: vec![running.id.clone()]
        }
    )));
    match done(a.outcome(&mut [], kill, Duration::from_secs(60))) {
        DoneView::Killed { results } => {
            assert_eq!(results.len(), 1);
            assert_eq!(results[0].id, running.id);
            assert_eq!(results[0].error, None);
        }
        other => panic!("running_kill: {other:?}"),
    }
    a.take(
        &mut [],
        "the killed session gone",
        Duration::from_secs(60),
        |r| match event(r)? {
            Event::Feed {
                value: FeedValue::Running { sessions, .. },
                ..
            } if sessions.iter().all(|s| s.id != running.id) => Some(()),
            _ => None,
        },
    );
    let _ = seq;
    no_paths(&a, &root);

    // -- unpair: answered, then revoked --
    let r = a.call(&mut [], None, Op::Unpair {});
    assert!(matches!(r, ReplyResult::Ok(Resp::Unit {})), "{r:?}");
    let client = sb.client();
    wait_for("the gateway link", Duration::from_secs(30), || {
        matches!(client.link(), Link::Up(_)).then_some(())
    });
    wait_mobile(&client, "the device revoked", |m| {
        !m.devices.iter().any(|d| d.id == phone.id) && m.devices.iter().any(|d| d.id == plain.id)
    });
    let end = Instant::now() + STEP;
    while a.closed.is_none() && Instant::now() < end {
        a.poll(SLICE);
    }
    assert!(a.closed.is_some(), "the unpaired device stays connected");
}

fn which(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(program))
        .find(|p| p.is_file())
}
