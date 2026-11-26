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
    if !support::isolated_device_test("slice4_device_ops_through_the_relay") {
        return;
    }
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
    assert_eq!(
        a.welcome.features,
        vec![
            api::FEATURE_S4.to_owned(),
            api::FEATURE_FORM.to_owned(),
            api::FEATURE_PROGRESS.to_owned(),
            api::FEATURE_TREE_NAV.to_owned(),
        ]
    );
    assert!(!a.welcome.push_key.is_empty(), "no push key in the welcome");
    let mut seq = a.welcome.next_seq;

    // -- Manage refused without the permission --
    let r = a.call(&mut [], Some(seq), Op::BackupConnect {});
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

    // -- the backup feed: the active destination, described without a secret --
    let r = a.call(
        &mut [],
        None,
        Op::Subscribe {
            feed: Feed::Backup {},
        },
    );
    assert!(matches!(r, ReplyResult::Ok(Resp::Unit {})), "{r:?}");
    let (destination, connected) = a.take(
        &mut [],
        "the backup feed",
        Duration::from_secs(60),
        |r| match event(r)? {
            Event::Feed {
                value:
                    FeedValue::Backup {
                        destination,
                        connected,
                        ..
                    },
                ..
            } => Some((destination, connected)),
            _ => None,
        },
    );
    assert_eq!(destination.kind, api::BackupKind::Drive);
    assert_eq!(destination.label, "Google Drive");
    assert!(!connected, "nobody signed in to Google in the sandbox");
    no_path_in(&destination, &root);

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

#[cfg(unix)]
mod tree_nav {
    use super::*;
    use serde_json::{Value, json};
    use std::io::{BufRead as _, BufReader, Write as _};
    use std::os::unix::{
        fs::PermissionsExt as _,
        net::{UnixListener, UnixStream},
    };
    use std::path::Path;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    const SEED: &str = "tree-seed";

    fn install_script(sb: &Sandbox) {
        let fake = sb.root.join("bin");
        std::fs::create_dir(&fake).unwrap();
        let omp = fake.join("omp");
        std::fs::write(
            &omp,
            r#"#!/bin/sh
case "$*" in
  *--version*) printf 'omp/19.2.1\n'; exit;;
  '--mode rpc --fork '*)
    file="$PI_CODING_AGENT_DIR/sessions/--proj--/tree-copy.jsonl"
    cp "$PI_CODING_AGENT_DIR/tree-copy.fixture" "$file" || exit 1
    while IFS= read -r line; do
      printf '{"type":"response","id":"2","success":true,"data":{"sessionFile":"%s"}}\n' "$file"
    done
    exit;;
  '--mode rpc '*) exit 1;;
esac
printf '%s\n' "$OMPDECK_CTL_DIR" > "$PI_CODING_AGENT_DIR/ctl-path"
exec sleep 120
"#,
        )
        .unwrap();
        std::fs::set_permissions(&omp, std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = std::env::join_paths(
            std::iter::once(fake).chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
        )
        .unwrap();
        // SAFETY: this case runs by itself, before any test worker is started.
        unsafe { std::env::set_var("PATH", path) };
    }

    fn write_entry(
        file: &mut std::fs::File,
        id: &str,
        parent: Option<&str>,
        role: &str,
        text: &str,
    ) {
        // Keep omp's on-disk key order: its entry discriminator is read at the line start.
        writeln!(
            file,
            r#"{{"type":"message","id":{},"parentId":{},"timestamp":"2026-10-29T00:00:01Z","message":{{"role":{},"content":{},"stopReason":"stop"}}}}"#,
            json!(id),
            json!(parent),
            json!(role),
            json!([{"type":"text","text":text}]),
        )
        .unwrap();
    }

    fn serve_ctl(mut stream: UnixStream, source: &Path, fork: &Path, fixture: &Path) {
        let snapshot = json!({
            "t":"agents", "pid":std::process::id(), "agents":[], "mainIdle":true,
            "sessionFile":source, "leaf":"a2", "tail":"a2"
        });
        if writeln!(
            stream,
            "{}\n{snapshot}",
            json!({"t":"hello","v":1,"pid":std::process::id()})
        )
        .is_err()
        {
            return;
        }
        for line in BufReader::new(stream.try_clone().unwrap())
            .lines()
            .map_while(Result::ok)
        {
            let cmd: Value = serde_json::from_str(&line).unwrap();
            let mut reply = json!({"t":"reply","rid":cmd["rid"],"ok":true});
            match cmd["t"].as_str().unwrap() {
                "navigate" => match cmd["entryId"].as_str().unwrap() {
                    "u4" => {
                        assert_eq!(cmd["takeEditor"], true);
                        reply["leaf"] = json!("a3");
                        reply["value"] = json!({"editorText":"retry wording","editorImages":1});
                        // A preceding snapshot and the reply may arrive in the same read.
                        if writeln!(stream, "{snapshot}\n{reply}").is_err() {
                            break;
                        }
                        continue;
                    }
                    "a1" => {
                        assert_ne!(cmd["takeEditor"], true);
                        reply["leaf"] = json!("a1");
                        let mut legacy = snapshot.clone();
                        legacy["tail"] = Value::Null;
                        if writeln!(stream, "{reply}\n{legacy}").is_err() {
                            break;
                        }
                        continue;
                    }
                    "a3" => {
                        assert_eq!(cmd["takeEditor"], true);
                        reply["leaf"] = json!("a3");
                        reply["tail"] = json!("u5");
                    }
                    _ => {
                        reply["ok"] = json!(false);
                        reply["error"] = json!("no such entry");
                    }
                },
                "fork_at" => {
                    assert_eq!(cmd["entryId"], "a1");
                    std::fs::copy(fixture, fork).unwrap();
                    reply["newFile"] = json!(fork);
                }
                "subscribe" => {}
                _ => {
                    reply["ok"] = json!(false);
                    reply["error"] = json!("unexpected scripted ctl command");
                }
            }
            if writeln!(stream, "{reply}").is_err() {
                break;
            }
        }
    }

    fn journaled(d: &mut Device, seq: &mut u64, op: Op) -> OutcomeView {
        let sent = *seq;
        *seq += 1;
        assert!(!accepted(d.call(&mut [], Some(sent), op)));
        d.outcome(&mut [], sent, STEP)
    }

    fn transcript(d: &mut Device) -> Vec<String> {
        d.take(&mut [], "a transcript reset", STEP, |r| match event(r)? {
            Event::Feed {
                value:
                    FeedValue::Transcript {
                        reset: true, items, ..
                    },
                ..
            } => Some(
                items
                    .into_iter()
                    .map(|item| match item {
                        api::Item::User { text, .. } | api::Item::Assistant { text, .. } => text,
                        other => panic!("unexpected transcript item {other:?}"),
                    })
                    .collect(),
            ),
            _ => None,
        })
    }

    #[test]
    fn scripted_phone_tree_and_fork_resume() {
        if !support::isolated_device_test("tree_nav::scripted_phone_tree_and_fork_resume") {
            return;
        }
        let sb = Sandbox::new("tn", OMP_CONFIG);
        sb.report_failures();
        install_script(&sb);
        sb.seed_session(SEED);
        let dir = sb.profile.sessions_root.join("--proj--");
        let source = dir.join(format!("{SEED}.jsonl"));
        let fork = dir.join("tree-fork.jsonl");
        let fixture = sb.agent().join("tree-fork.fixture");
        for (file, id) in [
            (&fixture, "tree-fork"),
            (&sb.agent().join("tree-copy.fixture"), "tree-copy"),
        ] {
            std::fs::write(file, format!("{}\n{}\n",
                json!({"type":"session","id":id,"cwd":sb.proj(),"timestamp":"2026-10-29T00:00:00Z"}),
                json!({"type":"title","title":"New session fixture"}),
            )).unwrap();
        }
        {
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&source)
                .unwrap();
            write_entry(&mut file, "u1", None, "user", "first");
            write_entry(&mut file, "a1", Some("u1"), "assistant", "first reply");
            write_entry(&mut file, "u3", Some("a1"), "user", "other branch");
            write_entry(&mut file, "a3", Some("u3"), "assistant", "same reply");
            write_entry(&mut file, "u4", Some("a3"), "user", "retry wording");
            write_entry(&mut file, "u2", Some("a1"), "user", "abandoned branch");
            write_entry(&mut file, "a2", Some("u2"), "assistant", "same reply");
        }
        let relay = Relay::start(sb.root.join("relay"));
        let client = sb.client();
        wait_for("the gateway link", STEP, || {
            matches!(client.link(), Link::Up(_)).then_some(())
        });
        run(
            &client,
            Mutation::RelaySet {
                url: relay.url(),
                room_key: SecretText::new(relay.key.encode()),
            },
        )
        .unwrap();
        wait_mobile(&client, "the relay link", |m| m.link == RelayLink::Up);
        let permissions = Permissions {
            control: true,
            manage: true,
        };
        let phone = pair(&client, &relay, "Tree phone", permissions);
        let old = pair(&client, &relay, "Legacy phone", permissions);
        let mut d = Device::connect_with(
            &relay,
            &phone,
            "Tree phone",
            &[api::FEATURE_S4, api::FEATURE_TREE_NAV],
        );
        let mut seq = d.welcome.next_seq;
        let DoneView::Session { live } = done(journaled(
            &mut d,
            &mut seq,
            Op::Resume {
                session: SEED.into(),
            },
        )) else {
            panic!("no resumed session");
        };
        let marker = sb.agent().join("ctl-path");
        let ctl = wait_for("the scripted session's control dir", STEP, || {
            let shown = std::fs::read_to_string(&marker).ok()?;
            (!shown.trim().is_empty()).then(|| PathBuf::from(shown.trim()))
        });
        let listener = UnixListener::bind(ctl.join("ctl.sock")).unwrap();
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let worker = {
            let stop = stop.clone();
            let source = source.clone();
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            let (source, fork, fixture) =
                                (source.clone(), fork.clone(), fixture.clone());
                            std::thread::spawn(move || serve_ctl(stream, &source, &fork, &fixture));
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(5))
                        }
                        Err(e) => panic!("accept ctl: {e}"),
                    }
                }
            })
        };
        assert!(matches!(
            d.call(
                &mut [],
                None,
                Op::Subscribe {
                    feed: Feed::Live {}
                }
            ),
            ReplyResult::Ok(Resp::Unit {})
        ));
        d.take(&mut [], "the scripted ctl link", STEP, |r| {
            match event(r)? {
                Event::Feed {
                    value: FeedValue::Live { sessions, .. },
                    ..
                } if sessions.iter().any(|s| s.live == live && s.live_control) => Some(()),
                _ => None,
            }
        });
        let feed = Feed::Transcript {
            session: SEED.into(),
            agent: None,
            tail: 20,
        };
        let subscribe = |d: &mut Device| {
            assert!(matches!(
                d.call(&mut [], None, Op::Subscribe { feed: feed.clone() }),
                ReplyResult::Ok(Resp::Unit {})
            ));
        };
        let unsubscribe = |d: &mut Device| {
            assert!(matches!(
                d.call(&mut [], None, Op::Unsubscribe { feed: feed.clone() }),
                ReplyResult::Ok(Resp::Unit {})
            ));
        };
        let drop_transcript_records = |d: &mut Device| {
            d.records.retain(|r| {
                !matches!(
                    event(r),
                    Some(Event::Feed {
                        value: FeedValue::Transcript { .. },
                        ..
                    })
                )
            });
        };
        subscribe(&mut d);
        assert_eq!(
            transcript(&mut d),
            ["first", "first reply", "abandoned branch", "same reply"]
        );

        let applied = seq;
        assert_eq!(
            done(journaled(
                &mut d,
                &mut seq,
                Op::Navigate {
                    live: live.clone(),
                    entry: "u4".into()
                }
            )),
            DoneView::Moved {
                leaf: Some("a3".into()),
                before_prompt: true,
                editor_text: Some("retry wording".into()),
                dropped_images: 1,
                message: None,
            }
        );
        assert!(matches!(
            d.call(&mut [], None, Op::Outcome { seq: applied }),
            ReplyResult::Ok(Resp::Outcome {
                outcome: OutcomeView::Done {
                    result: DoneView::Moved {
                        before_prompt: true,
                        ..
                    }
                }
            })
        ));
        // This branch has the same length and final item as the one it replaced.
        assert_eq!(
            transcript(&mut d),
            ["first", "first reply", "other branch", "same reply"]
        );

        let tree = |d: &mut Device, cursor| {
            d.call(
                &mut [],
                None,
                Op::Tree {
                    session: SEED.into(),
                    filter: api::TreeFilter::Conversation,
                    cursor,
                    limit: 2,
                    at_leaf: true,
                },
            )
        };
        let previous = match tree(&mut d, None) {
            ReplyResult::Ok(Resp::Tree {
                nodes,
                active_leaf,
                prev_cursor,
                next_cursor,
                total,
                ..
            }) => {
                assert_eq!(active_leaf.as_deref(), Some("a3"));
                assert_eq!(
                    nodes.iter().map(|n| n.id.as_str()).collect::<Vec<_>>(),
                    ["a3", "u4"]
                );
                assert_eq!(total, Some(7));
                assert_eq!(next_cursor.as_deref(), Some("t-5"));
                assert_eq!(prev_cursor.as_deref(), Some("b-3"));
                prev_cursor
            }
            other => panic!("centered tree page: {other:?}"),
        };
        match tree(&mut d, previous) {
            ReplyResult::Ok(Resp::Tree {
                nodes,
                prev_cursor,
                next_cursor,
                total,
                ..
            }) => {
                assert_eq!(
                    nodes.iter().map(|n| n.id.as_str()).collect::<Vec<_>>(),
                    ["a1", "u3"]
                );
                assert_eq!(prev_cursor.as_deref(), Some("b-1"));
                assert_eq!(next_cursor.as_deref(), Some("t-3"));
                assert_eq!(total, Some(7));
            }
            other => panic!("backwards tree page: {other:?}"),
        }
        match journaled(
            &mut d,
            &mut seq,
            Op::Navigate {
                live: live.clone(),
                entry: "missing".into(),
            },
        ) {
            OutcomeView::Failed { message } => assert_eq!(message, "no such entry"),
            other => panic!("refused navigate: {other:?}"),
        }
        let mut legacy = Device::connect(&relay, &old, "Legacy phone", true);
        let mut legacy_seq = legacy.welcome.next_seq;
        assert_eq!(
            done(journaled(
                &mut legacy,
                &mut legacy_seq,
                Op::Navigate {
                    live: live.clone(),
                    entry: "a1".into(),
                }
            )),
            DoneView::Unit {}
        );
        assert!(matches!(tree(&mut d, None),
            ReplyResult::Ok(Resp::Tree { active_leaf: Some(leaf), .. }) if leaf == "a2"));
        drop(legacy);

        // Recreating a watcher without an intervening append can replay the last reply.
        // Once the file advances, the old reply cannot truncate the watcher on creation.
        for append in [false, true] {
            unsubscribe(&mut d);
            drop_transcript_records(&mut d);
            assert!(matches!(
                done(journaled(
                    &mut d,
                    &mut seq,
                    Op::Navigate {
                        live: live.clone(),
                        entry: "u4".into(),
                    }
                )),
                DoneView::Moved { .. }
            ));
            if append {
                let mut file = std::fs::OpenOptions::new()
                    .append(true)
                    .open(&source)
                    .unwrap();
                write_entry(&mut file, "u5", Some("a2"), "user", "later append");
            }
            subscribe(&mut d);
            let expected: &[&str] = if append {
                &[
                    "first",
                    "first reply",
                    "abandoned branch",
                    "same reply",
                    "later append",
                ]
            } else {
                &["first", "first reply", "other branch", "same reply"]
            };
            assert_eq!(transcript(&mut d), expected);
        }

        unsubscribe(&mut d);
        drop_transcript_records(&mut d);
        let moved = done(journaled(
            &mut d,
            &mut seq,
            Op::Navigate {
                live: live.clone(),
                entry: "a3".into(),
            },
        ));
        assert!(matches!(moved, DoneView::Moved { leaf: Some(leaf), .. } if leaf == "a3"));
        assert!(matches!(tree(&mut d, None),
            ReplyResult::Ok(Resp::Tree { active_leaf: Some(leaf), .. }) if leaf == "a3"));
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&source)
            .unwrap();
        // An unresolved pointer after the tail must leave the exact anchor selected. A
        // file-only read would keep u5 on the abandoned branch, so this checks that the
        // gateway really retained the anchored reply across the append and recreation.
        writeln!(
            file,
            "{}",
            json!({"type":"branch_summary","id":"marker","parentId":"a3","activeLeafId":"missing"})
        )
        .unwrap();
        subscribe(&mut d);
        assert_eq!(
            transcript(&mut d),
            ["first", "first reply", "other branch", "same reply"]
        );

        unsubscribe(&mut d);
        drop_transcript_records(&mut d);
        write_entry(
            &mut file,
            "u6",
            Some("a3"),
            "user",
            "after the anchored jump",
        );
        subscribe(&mut d);
        assert_eq!(
            transcript(&mut d),
            [
                "first",
                "first reply",
                "other branch",
                "same reply",
                "after the anchored jump"
            ]
        );

        for (op, id) in [
            (
                Op::ForkAt {
                    session: SEED.into(),
                    entry: "a1".into(),
                },
                "tree-fork",
            ),
            (
                Op::CopySession {
                    session: SEED.into(),
                    project: api::project_id(sb.proj().to_string_lossy().as_bytes()),
                },
                "tree-copy",
            ),
        ] {
            let result = done(journaled(&mut d, &mut seq, op));
            let DoneView::File {
                session: Some(session),
            } = result
            else {
                panic!("new file has no session: {result:?}");
            };
            assert_eq!(session, id);
            assert!(matches!(
                done(journaled(&mut d, &mut seq, Op::Resume { session })),
                DoneView::Session { .. }
            ));
        }
        stop.store(true, Ordering::Relaxed);
        worker.join().unwrap();
    }
}

fn which(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(program))
        .find(|p| p.is_file())
}
