//! Boundary tests against an in-process mock owner on the real private transport.

use std::io;
use std::sync::Arc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use deck_core::config::GatewayMode;
use deck_host::transport::{Endpoint, Listener, Stream};
use deck_term::{InputMeta, RemoteEvent};

use super::*;
use crate::controller::SessionKey;
use crate::gateway::protocol::{Attach, InboxView, TopicValue};

const TIMEOUT: Duration = Duration::from_secs(15);

fn test_profile(tag: &str) -> (tempfile::TempDir, Profile) {
    let dir = tempfile::tempdir().expect("temp dir");
    let profile = Profile {
        host_namespace: format!("omp-deck-test-{}-{tag}", std::process::id()),
        settings: dir.path().join("state.json"),
        sessions_root: dir.path().join("sessions"),
    };
    (dir, profile)
}

fn endpoint(p: &Profile) -> Endpoint {
    let ns = p.id().namespace().expect("namespace");
    Endpoint::new(&ns, OWNER_ENDPOINT).expect("endpoint")
}

fn options(profile: &Profile, role: Role) -> GatewayOptions {
    let mut o = GatewayOptions::new(profile.clone(), role, Arc::new(|| {}));
    o.autostart = false;
    o
}

/// Accepts the next connection within [`TIMEOUT`].
fn accept(listener: &Arc<Listener>) -> Stream {
    let (tx, rx) = mpsc::channel();
    let l = listener.clone();
    std::thread::spawn(move || {
        let _ = tx.send(l.accept());
    });
    let s = rx
        .recv_timeout(TIMEOUT)
        .expect("the client connected in time")
        .expect("accept");
    s.set_read_timeout(Some(TIMEOUT)).expect("timeout");
    s
}

fn hello(s: &mut Stream) -> Hello {
    match wire::read_frame(s).expect("read").expect("frame") {
        (kind::HELLO, p) => serde_json::from_slice(&p).expect("hello"),
        (k, _) => panic!("expected HELLO, got {k}"),
    }
}

fn welcome(s: &mut Stream, epoch: ServiceEpoch, profile: &Profile) {
    welcome_as(s, epoch, profile, GATEWAY_VERSION);
}

fn welcome_as(s: &mut Stream, epoch: ServiceEpoch, profile: &Profile, version: &str) {
    let w = Welcome {
        protocol: PROTOCOL,
        version: version.into(),
        epoch,
        pid: 1,
        profile: profile.clone(),
        mode: GatewayMode::Off,
        maintenance: None,
    };
    wire::write_frame(s, kind::WELCOME, &serde_json::to_vec(&w).expect("json")).expect("welcome");
}

fn request(s: &mut Stream, asm: &mut Assembly) -> io::Result<Envelope> {
    loop {
        match wire::read_frame(s)? {
            Some((kind::REQUEST, chunk)) => {
                if let Some((_, bytes)) = asm.push(&chunk)? {
                    return Ok(serde_json::from_slice(&bytes).expect("envelope"));
                }
            }
            Some((k, _)) => panic!("unexpected frame kind {k}"),
            None => return Err(io::ErrorKind::UnexpectedEof.into()),
        }
    }
}

fn reply(s: &mut Stream, id: u64, r: &Reply) {
    let bytes = serde_json::to_vec(r).expect("json");
    for chunk in wire::chunks(id, &bytes) {
        wire::write_frame(s, kind::REPLY, &chunk).expect("reply");
    }
}

fn event(s: &mut Stream, ev: &ServerEvent) {
    wire::write_frame(s, kind::EVENT, &serde_json::to_vec(ev).expect("json")).expect("event");
}

fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + TIMEOUT;
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Reads requests, answering each with `Done`, until `pred` matches one (returned unanswered).
fn until(s: &mut Stream, asm: &mut Assembly, mut pred: impl FnMut(&Request) -> bool) -> Envelope {
    loop {
        let env = request(s, asm).expect("request");
        if pred(&env.body) {
            return env;
        }
        reply(s, env.id, &Ok(Answer::Done));
    }
}

#[test]
fn a_dead_owner_fails_pending_requests_and_the_next_owner_gets_the_interests_back() {
    let (_dir, profile) = test_profile("reconnect");
    let ep = endpoint(&profile);
    let listener = Arc::new(Listener::bind(&ep).expect("bind"));
    let client = GatewayClient::start(options(&profile, Role::Cli));
    client.subscribe(Topic::Inventory);
    client.set_client_state(ClientState {
        focused: true,
        ..ClientState::default()
    });

    let mut s = accept(&listener);
    let h = hello(&mut s);
    assert_eq!(h.client_id, client.client_id());
    assert!(h.attach.is_none());
    welcome(&mut s, ServiceEpoch([1; 16]), &profile);
    let mut asm = Assembly::default();
    until(&mut s, &mut asm, |r| {
        matches!(r, Request::Subscribe(Topic::Inventory))
    });
    wait_until("link up", || matches!(client.link(), Link::Up(_)));
    assert_eq!(client.epoch(), Some(ServiceEpoch([1; 16])));

    let q = client.query(Query::OmpVersion);
    let m = client.mutate(Mutation::InboxClear);
    let env = request(&mut s, &mut asm).expect("request");
    assert!(matches!(
        env.body,
        Request::Query(Query::Topic(Topic::Inventory))
    ));
    let env = request(&mut s, &mut asm).expect("request");
    assert!(matches!(env.body, Request::Query(Query::OmpVersion)));
    let env = request(&mut s, &mut asm).expect("request");
    assert!(
        matches!(&env.body, Request::Mutate { key, epoch, .. }
            if *key == m.key && *epoch == ServiceEpoch([1; 16])),
        "mutations are bound to the connected epoch"
    );
    // All three unanswered; the owner dies.
    listener.remove();
    drop(listener);
    s.shutdown();
    drop(s);

    let r = q.wait(TIMEOUT);
    assert!(matches!(r, Some(Err(GatewayError::Disconnected))), "{r:?}");
    let a = m.accepted.wait(TIMEOUT);
    assert!(matches!(a, Some(Err(GatewayError::Disconnected))), "{a:?}");
    assert!(matches!(
        m.outcome.wait(TIMEOUT),
        Some(Err(GatewayError::Disconnected))
    ));
    wait_until("link down", || matches!(client.link(), Link::Down { .. }));
    assert_eq!(client.epoch(), None);
    // Nothing is queued for a later link.
    let offline = client.mutate(Mutation::InboxClear);
    assert!(matches!(
        offline.accepted.try_take(),
        Some(Err(GatewayError::Disconnected))
    ));

    let listener = Arc::new(Listener::bind(&ep).expect("rebind"));
    let mut s = accept(&listener);
    hello(&mut s);
    welcome(&mut s, ServiceEpoch([2; 16]), &profile);
    let mut asm = Assembly::default();
    let mut restored_state = false;
    until(&mut s, &mut asm, |r| {
        restored_state |= matches!(r, Request::ClientState(c) if c.focused);
        matches!(r, Request::Subscribe(Topic::Inventory))
    });
    assert!(
        restored_state,
        "client state is sent again before the subscriptions"
    );
    wait_until("new epoch", || {
        client.epoch() == Some(ServiceEpoch([2; 16]))
    });
    let links: Vec<_> = client
        .events()
        .into_iter()
        .filter_map(|e| match e {
            ClientEvent::Link(l) => Some(l),
            _ => None,
        })
        .collect();
    assert!(matches!(links.last(), Some(Link::Up(w)) if w.epoch == ServiceEpoch([2; 16])));
    assert!(links.iter().any(|l| matches!(l, Link::Down { .. })));
}

/// An autostarting GUI asks an owner of an older build to exit once it is drained (never
/// while it has work), and only once: an owner that comes back still older is kept.
#[test]
fn an_older_owner_is_asked_to_exit_once_drained_and_only_once() {
    let (_dir, profile) = test_profile("outdated");
    let listener = Arc::new(Listener::bind(&endpoint(&profile)).expect("bind"));
    let mut o = options(&profile, Role::Gui);
    o.autostart = true;
    // Never launched: the mock owner stays reachable.
    o.gateway_exe = Some(PathBuf::from("/nonexistent/omp-deck-gateway"));
    let client = GatewayClient::start(o);
    let is_maintenance = |r: &Request| matches!(r, Request::Maintenance(_));

    // Busy: polled, not told to exit.
    let mut s = accept(&listener);
    hello(&mut s);
    welcome_as(&mut s, ServiceEpoch([1; 16]), &profile, "0.0.1");
    let mut asm = Assembly::default();
    let env = request(&mut s, &mut asm).expect("request");
    assert!(matches!(env.body, Request::Maintenance(MaintenanceOp::Poll)), "{:?}", env.body);
    let busy = DrainState::Draining {
        jobs: 1,
        controllers_busy: 0,
        direct_programs: 0,
    };
    reply(&mut s, env.id, &Ok(Answer::Maintenance(busy)));
    wait_until("link up", || matches!(client.link(), Link::Up(_)));
    let _sentinel = client.query(Query::OmpVersion);
    let env = until(&mut s, &mut asm, |r| {
        is_maintenance(r) || matches!(r, Request::Query(Query::OmpVersion))
    });
    assert!(!is_maintenance(&env.body), "{:?}", env.body);
    s.shutdown();
    drop(s);

    // Drained: told to exit.
    let mut s = accept(&listener);
    hello(&mut s);
    welcome_as(&mut s, ServiceEpoch([2; 16]), &profile, "0.0.1");
    let mut asm = Assembly::default();
    let env = request(&mut s, &mut asm).expect("request");
    assert!(matches!(env.body, Request::Maintenance(MaintenanceOp::Poll)), "{:?}", env.body);
    reply(&mut s, env.id, &Ok(Answer::Maintenance(DrainState::Drained)));
    let env = until(&mut s, &mut asm, is_maintenance);
    assert!(matches!(env.body, Request::Maintenance(MaintenanceOp::Exit)), "{:?}", env.body);
    reply(&mut s, env.id, &Ok(Answer::Done));
    s.shutdown();
    drop(s);

    // The owner that comes back is still older (another installation's): kept.
    let mut s = accept(&listener);
    hello(&mut s);
    welcome_as(&mut s, ServiceEpoch([3; 16]), &profile, "0.0.1");
    let mut asm = Assembly::default();
    let env = request(&mut s, &mut asm).expect("request");
    assert!(!is_maintenance(&env.body), "{:?}", env.body);
    drop(client);
}

fn inbox(badge: usize) -> Reply {
    Ok(Answer::Topic(Box::new(TopicValue::Notifications(
        InboxView {
            entries: Vec::new(),
            warns: false,
            badge: Some(badge.to_string()),
        },
    ))))
}

fn badge(c: &GatewayClient) -> Option<String> {
    c.state().notifications.get().and_then(|v| v.badge.clone())
}

/// Answers topic fetches until `n` arrived: the inbox with `badge`, the others with an error.
fn answer_fetches(s: &mut Stream, asm: &mut Assembly, n: usize, badge: usize) -> usize {
    let mut inbox_fetches = 0;
    for _ in 0..n {
        let env = request(s, asm).expect("request");
        match env.body {
            Request::Query(Query::Topic(Topic::Notifications)) => {
                inbox_fetches += 1;
                reply(s, env.id, &inbox(badge));
            }
            Request::Query(Query::Topic(_)) => reply(
                s,
                env.id,
                &Err(GatewayError::Failed("not in this test".into())),
            ),
            other => panic!("unexpected request {other:?}"),
        }
    }
    inbox_fetches
}

#[test]
fn changes_are_refetched_coalesced_and_a_resync_refetches_everything() {
    let (_dir, profile) = test_profile("resync");
    let listener = Arc::new(Listener::bind(&endpoint(&profile)).expect("bind"));
    let client = GatewayClient::start(options(&profile, Role::Gui));
    let mut s = accept(&listener);
    hello(&mut s);
    welcome(&mut s, ServiceEpoch([4; 16]), &profile);
    let mut asm = Assembly::default();

    // A GUI fetches every always-on topic after the welcome.
    assert_eq!(answer_fetches(&mut s, &mut asm, ALWAYS_ON.len(), 1), 1);
    wait_until("first inbox", || badge(&client).as_deref() == Some("1"));
    let rev1 = client.state().revision(&Topic::Notifications);

    // Three changes while one fetch is in flight: one fetch now, exactly one after it.
    for revision in 1..=3 {
        event(
            &mut s,
            &ServerEvent::Changed {
                topic: Topic::Notifications,
                revision,
            },
        );
    }
    let first = request(&mut s, &mut asm).expect("refetch");
    assert!(matches!(
        first.body,
        Request::Query(Query::Topic(Topic::Notifications))
    ));
    s.set_read_timeout(Some(Duration::from_millis(300)))
        .expect("timeout");
    assert!(
        request(&mut s, &mut asm).is_err(),
        "no second fetch while one is in flight"
    );
    s.set_read_timeout(Some(TIMEOUT)).expect("timeout");
    reply(&mut s, first.id, &inbox(2));
    assert_eq!(answer_fetches(&mut s, &mut asm, 1, 3), 1);
    wait_until("coalesced refetch", || {
        badge(&client).as_deref() == Some("3")
    });
    assert!(client.state().revision(&Topic::Notifications) > rev1);
    assert!(client.events().iter().any(|e| matches!(
        e,
        ClientEvent::Changed { topic: Topic::Notifications, revision } if *revision > rev1
    )));

    // The gateway dropped events for this client: everything is fetched again.
    event(&mut s, &ServerEvent::Resync);
    assert_eq!(answer_fetches(&mut s, &mut asm, ALWAYS_ON.len(), 4), 1);
    wait_until("resynced inbox", || badge(&client).as_deref() == Some("4"));
    assert!(
        client
            .events()
            .iter()
            .any(|e| matches!(e, ClientEvent::Resync))
    );
}

/// Accepts the next frontend connection (control links of the client are turned away).
fn accept_view(listener: &Arc<Listener>) -> (Stream, Hello) {
    loop {
        let mut s = accept(listener);
        let h = hello(&mut s);
        if h.attach.is_some() {
            return (s, h);
        }
        s.shutdown();
    }
}

fn snapshot(s: &mut Stream, seq: u64, bytes: &[u8]) {
    for chunk in wire::chunks(seq, bytes) {
        wire::write_frame(s, kind::F_SNAPSHOT, &chunk).expect("snapshot");
    }
}

fn next_input(s: &mut Stream) -> Vec<u8> {
    loop {
        let (k, p) = wire::read_frame(s).expect("read").expect("frame");
        if k == kind::U_INPUT {
            return protocol::decode_input(&p).expect("input").1.to_vec();
        }
    }
}

#[test]
fn view_input_is_dropped_while_the_link_is_down_and_a_fresh_snapshot_reopens_it() {
    let (_dir, profile) = test_profile("view");
    let listener = Arc::new(Listener::bind(&endpoint(&profile)).expect("bind"));
    let target = SessionTarget {
        epoch: ServiceEpoch([3; 16]),
        key: SessionKey::direct(),
        generation: 1,
    };
    let client = GatewayClient::start(options(&profile, Role::Cli));
    let fe = client.frontend(target.clone(), ViewRole::Controlling);
    drop(client);
    let (mut rx, tx) = fe.split();
    let (ev_tx, ev_rx) = mpsc::channel();
    std::thread::spawn(move || {
        loop {
            let ev = rx.recv().unwrap_or(RemoteEvent::Closed);
            let end = matches!(ev, RemoteEvent::Closed);
            if ev_tx.send(ev).is_err() || end {
                return;
            }
        }
    });
    let next = || ev_rx.recv_timeout(TIMEOUT).expect("event");

    let (mut s, h) = accept_view(&listener);
    assert_eq!(
        h.attach,
        Some(Attach {
            target: target.clone(),
            view: ViewRole::Controlling
        })
    );
    welcome(&mut s, target.epoch, &profile);
    snapshot(&mut s, 1, b"snap-1");
    assert_eq!(next(), RemoteEvent::Snapshot(b"snap-1".to_vec()));
    wire::write_frame(&mut s, kind::F_OUTPUT, b"out").expect("output");
    assert_eq!(next(), RemoteEvent::Output(b"out".to_vec()));
    tx.input(b"a", InputMeta::default())
        .expect("input while up");
    assert_eq!(next_input(&mut s), b"a");

    // The owner side goes away: one Disconnected, and typing is refused, not queued.
    s.shutdown();
    assert_eq!(next(), RemoteEvent::Disconnected);
    assert!(tx.input(b"b", InputMeta::default()).is_err());

    let (mut s, _) = accept_view(&listener);
    welcome(&mut s, target.epoch, &profile);
    snapshot(&mut s, 1, b"snap-2");
    assert_eq!(next(), RemoteEvent::Snapshot(b"snap-2".to_vec()));
    tx.input(b"c", InputMeta::default())
        .expect("input after re-attach");
    assert_eq!(next_input(&mut s), b"c", "the dropped input never arrives");

    tx.detach().expect("detach");
    let (k, _) = wire::read_frame(&mut s).expect("read").expect("frame");
    assert_eq!(k, kind::U_DETACH);
    assert_eq!(next(), RemoteEvent::Closed);
}
