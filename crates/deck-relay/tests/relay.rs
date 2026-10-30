//! End-to-end relay tests: real sockets on 127.0.0.1, tungstenite clients, shrunken limits.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use deck_relay::{Config, RateLimit, RelayError, rooms};
use deck_remote::relay::{
    self, DEVICE_PATH, FromRelay, GATEWAY_PATH, HELLO_OK, Hash, Hello, MAX_PAYLOAD, PeerId,
    RoomKey, ToRelay, Token, close,
};
use futures_util::{SinkExt as _, StreamExt as _};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};
use tokio::net::{TcpSocket, TcpStream};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep, timeout};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
use tokio_tungstenite::tungstenite::{self, Message};

const WAIT: Duration = Duration::from_secs(5);

struct Relay {
    addr: SocketAddr,
    dir: tempfile::TempDir,
    key: RoomKey,
    stop: Option<oneshot::Sender<()>>,
    task: JoinHandle<Result<(), RelayError>>,
}

impl Relay {
    async fn start(tweak: impl FnOnce(&mut Config)) -> Relay {
        let dir = tempfile::tempdir().unwrap();
        let (key, _) = rooms::enroll(dir.path(), "test").unwrap();
        let mut config = Config::new(dir.path());
        config.close_timeout = Duration::from_secs(1);
        tweak(&mut config);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (stop, stopped) = oneshot::channel::<()>();
        let task = tokio::spawn(deck_relay::serve(config, listener, async move {
            let _ = stopped.await;
        }));
        Relay {
            addr,
            dir,
            key,
            stop: Some(stop),
            task,
        }
    }

    fn state(&self) -> &Path {
        self.dir.path()
    }

    async fn ws(&self, path: &str) -> Ws {
        let tcp = TcpStream::connect(self.addr).await.unwrap();
        tcp.set_nodelay(true).unwrap();
        let (ws, _) = tokio_tungstenite::client_async(format!("ws://{}{path}", self.addr), tcp)
            .await
            .unwrap();
        ws
    }

    async fn gateway_with(&self, key: &RoomKey) -> Ws {
        self.gateway_hello(key, 0).await
    }

    async fn gateway_hello(&self, key: &RoomKey, epoch: u64) -> Ws {
        let mut ws = self.ws(GATEWAY_PATH).await;
        send(
            &mut ws,
            Hello::Gateway {
                room: key.room,
                secret: key.secret,
                epoch,
            }
            .encode(),
        )
        .await;
        ws
    }

    /// An admitted gateway that registered `tokens`.
    async fn gateway(&self, tokens: &[Token]) -> Ws {
        self.gateway_at(0, tokens).await
    }

    /// An admitted gateway with hello `epoch` that registered `tokens` (and the relay applied them).
    async fn gateway_at(&self, epoch: u64, tokens: &[Token]) -> Ws {
        let mut ws = self.gateway_hello(&self.key, epoch).await;
        assert_eq!(recv(&mut ws).await, HELLO_OK);
        set_tokens(&mut ws, tokens).await;
        self.tokens_applied().await;
        ws
    }

    /// Waits until the room's gateway registered a token set: before that a device hello is
    /// refused `GATEWAY_OFFLINE`, after it a probe with an unknown token gets `UNAUTHORIZED`.
    async fn tokens_applied(&self) {
        let deadline = Instant::now() + WAIT;
        loop {
            let mut probe = self.device_hello(PROBE).await;
            let code = closed(&mut probe).await;
            finish(probe).await;
            match code {
                close::UNAUTHORIZED => return,
                close::GATEWAY_OFFLINE => {
                    assert!(Instant::now() < deadline, "token set never applied");
                    sleep(Duration::from_millis(10)).await;
                }
                other => panic!("unexpected probe close {other}"),
            }
        }
    }

    async fn device_hello(&self, token: Token) -> Ws {
        let mut ws = self.ws(DEVICE_PATH).await;
        send(
            &mut ws,
            Hello::Device {
                room: self.key.room,
                token,
            }
            .encode(),
        )
        .await;
        ws
    }

    /// An admitted device; returns its peer id from the gateway's `Open`.
    async fn device(&self, gateway: &mut Ws, token: Token) -> (Ws, PeerId) {
        let mut ws = self.device_hello(token).await;
        assert_eq!(recv(&mut ws).await, HELLO_OK);
        let open = recv(gateway).await;
        match FromRelay::decode(&open).unwrap() {
            FromRelay::Open { peer, token: h } => {
                assert_eq!(h, relay::hash(&token));
                (ws, peer)
            }
            other => panic!("expected Open, got {other:?}"),
        }
    }

    async fn stop(mut self) -> Result<(), RelayError> {
        let _ = self.stop.take().unwrap().send(());
        timeout(Duration::from_secs(6), &mut self.task)
            .await
            .expect("serve returns within the shutdown timeout")
            .unwrap()
    }
}

type Ws = WebSocketStream<TcpStream>;

async fn send<S: AsyncRead + AsyncWrite + Unpin>(ws: &mut WebSocketStream<S>, bytes: Vec<u8>) {
    ws.send(Message::Binary(bytes.into())).await.unwrap();
}

async fn set_tokens(gateway: &mut Ws, tokens: &[Token]) {
    let hashes: Vec<Hash> = tokens.iter().map(relay::hash).collect();
    send(gateway, ToRelay::Tokens(hashes).encode().unwrap()).await;
}

/// Never in a token set: probes whether a gateway's set is applied.
const PROBE: Token = [0xee; 32];

/// After the close frame: answers it (tungstenite sends the reply on the next read) and waits
/// until the relay dropped the socket, so its connection slot is free again.
async fn finish(mut ws: Ws) {
    let _ = timeout(WAIT, async { while let Some(Ok(_)) = ws.next().await {} }).await;
    let mut buf = [0u8; 1];
    let _ = timeout(WAIT, ws.get_mut().read(&mut buf)).await;
}

/// The next binary message (pings are skipped); panics on close.
async fn recv<S: AsyncRead + AsyncWrite + Unpin>(ws: &mut WebSocketStream<S>) -> Vec<u8> {
    loop {
        match timeout(WAIT, ws.next()).await.expect("message in time") {
            Some(Ok(Message::Binary(b))) => return b.to_vec(),
            Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
            other => panic!("expected a binary message, got {other:?}"),
        }
    }
}

/// Reads until the close frame and returns its code (binary messages before it are skipped).
async fn closed<S: AsyncRead + AsyncWrite + Unpin>(ws: &mut WebSocketStream<S>) -> u16 {
    let deadline = Instant::now() + WAIT;
    loop {
        match tokio::time::timeout_at(deadline, ws.next())
            .await
            .expect("close in time")
        {
            Some(Ok(Message::Close(frame))) => {
                return frame.map_or(0, |f| u16::from(f.code));
            }
            Some(Ok(_)) => {}
            other => panic!("expected a close frame, got {other:?}"),
        }
    }
}

/// Nothing arrives for `wait` (pings aside).
async fn quiet(ws: &mut Ws, wait: Duration) {
    let deadline = Instant::now() + wait;
    loop {
        match tokio::time::timeout_at(deadline, ws.next()).await {
            Err(_) => return,
            Ok(Some(Ok(Message::Ping(_) | Message::Pong(_)))) => {}
            Ok(other) => panic!("expected silence, got {other:?}"),
        }
    }
}

fn data(peer: PeerId, payload: &[u8]) -> Vec<u8> {
    ToRelay::Data { peer, payload }.encode().unwrap()
}

const T1: Token = [1; 32];
const T2: Token = [2; 32];

#[tokio::test]
async fn gateway_and_device_round_trip_with_open_and_closed() {
    let r = Relay::start(|_| {}).await;
    let mut g = r.gateway(&[T1, T2]).await;
    let (mut d1, p1) = r.device(&mut g, T1).await;
    let (mut d2, p2) = r.device(&mut g, T2).await;
    assert_ne!(p1, p2);

    send(&mut d1, b"from one".to_vec()).await;
    assert_eq!(
        FromRelay::decode(&recv(&mut g).await).unwrap(),
        FromRelay::Data {
            peer: p1,
            payload: b"from one"
        }
    );
    send(&mut d2, b"from two".to_vec()).await;
    assert_eq!(
        FromRelay::decode(&recv(&mut g).await).unwrap(),
        FromRelay::Data {
            peer: p2,
            payload: b"from two"
        }
    );
    send(&mut g, data(p2, b"to two")).await;
    send(&mut g, data(p1, b"to one")).await;
    send(&mut g, data(p1, &[])).await;
    // Unknown peers are dropped silently.
    send(&mut g, data(999, b"nobody")).await;
    assert_eq!(recv(&mut d1).await, b"to one");
    assert_eq!(recv(&mut d1).await, b"");
    assert_eq!(recv(&mut d2).await, b"to two");

    d1.close(None).await.unwrap();
    assert_eq!(
        FromRelay::decode(&recv(&mut g).await).unwrap(),
        FromRelay::Closed { peer: p1 }
    );
    // Closed came once: the next gateway message is the next device's Open, with a fresh id.
    let (_d3, p3) = r.device(&mut g, T1).await;
    assert!(p3 != p1 && p3 != p2);

    // Gateway-requested close: the code is passed on (non-application codes become NORMAL).
    send(
        &mut g,
        ToRelay::Close {
            peer: p2,
            code: 4321,
        }
        .encode()
        .unwrap(),
    )
    .await;
    assert_eq!(closed(&mut d2).await, 4321);
    assert_eq!(
        FromRelay::decode(&recv(&mut g).await).unwrap(),
        FromRelay::Closed { peer: p2 }
    );
    let (mut d4, p4) = r.device(&mut g, T2).await;
    send(
        &mut g,
        ToRelay::Close {
            peer: p4,
            code: 3000,
        }
        .encode()
        .unwrap(),
    )
    .await;
    assert_eq!(closed(&mut d4).await, close::NORMAL);
    r.stop().await.unwrap();
}

#[tokio::test]
async fn gateway_hello_needs_an_enrolled_room_and_its_secret() {
    let r = Relay::start(|_| {}).await;
    let wrong_secret = RoomKey {
        room: r.key.room,
        secret: [9; 32],
    };
    let mut g = r.gateway_with(&wrong_secret).await;
    assert_eq!(closed(&mut g).await, close::UNAUTHORIZED);
    let unknown = RoomKey {
        room: [7; 16],
        secret: r.key.secret,
    };
    let mut g = r.gateway_with(&unknown).await;
    assert_eq!(closed(&mut g).await, close::UNAUTHORIZED);
    // A room enrolled while the relay runs is picked up at the next gateway hello.
    let (key, _) = rooms::enroll(r.state(), "later").unwrap();
    let mut g = r.gateway_with(&key).await;
    assert_eq!(recv(&mut g).await, HELLO_OK);
    r.stop().await.unwrap();
}

#[tokio::test]
async fn bad_hellos_are_rejected() {
    let r = Relay::start(|c| c.hello_timeout = Duration::from_millis(300)).await;
    // Device before its gateway.
    let mut d = r.device_hello(T1).await;
    assert_eq!(closed(&mut d).await, close::GATEWAY_OFFLINE);
    // Device hello on the gateway path, and the reverse.
    let mut g = r.ws(GATEWAY_PATH).await;
    send(
        &mut g,
        Hello::Device {
            room: r.key.room,
            token: T1,
        }
        .encode(),
    )
    .await;
    assert_eq!(closed(&mut g).await, close::BAD_HELLO);
    let mut d = r.ws(DEVICE_PATH).await;
    send(
        &mut d,
        Hello::Gateway {
            room: r.key.room,
            secret: r.key.secret,
            epoch: 0,
        }
        .encode(),
    )
    .await;
    assert_eq!(closed(&mut d).await, close::BAD_HELLO);
    // Undecodable.
    let mut d = r.ws(DEVICE_PATH).await;
    send(&mut d, b"hello?".to_vec()).await;
    assert_eq!(closed(&mut d).await, close::BAD_HELLO);
    // Text.
    let mut d = r.ws(DEVICE_PATH).await;
    d.send(Message::text("hi")).await.unwrap();
    assert_eq!(closed(&mut d).await, close::PROTOCOL);
    // Silence.
    let mut d = r.ws(DEVICE_PATH).await;
    let started = Instant::now();
    assert_eq!(closed(&mut d).await, close::BAD_HELLO);
    assert!(started.elapsed() >= Duration::from_millis(250));
    r.stop().await.unwrap();
}

#[tokio::test]
async fn admission_follows_the_gateway_token_set() {
    let r = Relay::start(|_| {}).await;
    let mut g = r.gateway(&[]).await;
    let mut d = r.device_hello(T1).await;
    assert_eq!(closed(&mut d).await, close::UNAUTHORIZED);
    set_tokens(&mut g, &[T1, T2]).await;
    let (mut d1, p1) = r.device(&mut g, T1).await;
    let (mut d2, p2) = r.device(&mut g, T2).await;

    // Dropping T1 closes its device only.
    set_tokens(&mut g, &[T2]).await;
    assert_eq!(closed(&mut d1).await, close::UNAUTHORIZED);
    assert_eq!(
        FromRelay::decode(&recv(&mut g).await).unwrap(),
        FromRelay::Closed { peer: p1 }
    );
    send(&mut d2, b"still here".to_vec()).await;
    assert_eq!(
        FromRelay::decode(&recv(&mut g).await).unwrap(),
        FromRelay::Data {
            peer: p2,
            payload: b"still here"
        }
    );
    let mut d = r.device_hello(T1).await;
    assert_eq!(closed(&mut d).await, close::UNAUTHORIZED);
    r.stop().await.unwrap();
}

#[tokio::test]
async fn devices_wait_for_the_gateway_token_set() {
    let r = Relay::start(|_| {}).await;
    // Between the gateway hello and its first token set: retryable, not "revoked".
    let mut g = r.gateway_hello(&r.key, 0).await;
    assert_eq!(recv(&mut g).await, HELLO_OK);
    let mut d = r.device_hello(T1).await;
    assert_eq!(closed(&mut d).await, close::GATEWAY_OFFLINE);
    // After it: admitted or unauthorized as usual.
    set_tokens(&mut g, &[T1]).await;
    r.tokens_applied().await;
    let (_d1, _) = r.device(&mut g, T1).await;
    let mut d = r.device_hello(T2).await;
    assert_eq!(closed(&mut d).await, close::UNAUTHORIZED);

    // A replacing gateway starts without a token set again.
    let mut g2 = r.gateway_hello(&r.key, 0).await;
    assert_eq!(recv(&mut g2).await, HELLO_OK);
    assert_eq!(closed(&mut g).await, close::REPLACED);
    let mut d = r.device_hello(T1).await;
    assert_eq!(closed(&mut d).await, close::GATEWAY_OFFLINE);
    set_tokens(&mut g2, &[T2]).await;
    r.tokens_applied().await;
    let (_d2, _) = r.device(&mut g2, T2).await;
    let mut d = r.device_hello(T1).await;
    assert_eq!(closed(&mut d).await, close::UNAUTHORIZED);
    r.stop().await.unwrap();
}

#[tokio::test]
async fn a_new_gateway_replaces_the_old_and_a_leaving_gateway_closes_its_devices() {
    let r = Relay::start(|_| {}).await;
    let mut g1 = r.gateway(&[T1]).await;
    let (mut d1, _) = r.device(&mut g1, T1).await;
    let mut g2 = r.gateway(&[T1]).await;
    assert_eq!(closed(&mut g1).await, close::REPLACED);
    assert_eq!(closed(&mut d1).await, close::GATEWAY_OFFLINE);
    // The admitted set starts empty after a hello: g2 registered T1 itself.
    let (mut d2, p2) = r.device(&mut g2, T1).await;
    send(&mut g2, data(p2, b"hi")).await;
    assert_eq!(recv(&mut d2).await, b"hi");

    g2.close(None).await.unwrap();
    assert_eq!(closed(&mut d2).await, close::GATEWAY_OFFLINE);
    let mut d = r.device_hello(T1).await;
    assert_eq!(closed(&mut d).await, close::GATEWAY_OFFLINE);
    r.stop().await.unwrap();
}

#[tokio::test]
async fn gateway_epochs_fence_stale_hellos() {
    let r = Relay::start(|_| {}).await;
    // (a) A newer epoch replaces the older gateway.
    let mut g1 = r.gateway_at(10, &[T1]).await;
    let mut g2 = r.gateway_at(20, &[T1]).await;
    assert_eq!(closed(&mut g1).await, close::REPLACED);

    // (b) A stale hello after the newer gateway is admitted is refused; the current gateway keeps
    // its devices and keeps serving.
    let (mut d, p) = r.device(&mut g2, T1).await;
    let mut stale = r.gateway_hello(&r.key, 15).await;
    assert_eq!(closed(&mut stale).await, close::REPLACED);
    send(&mut d, b"up".to_vec()).await;
    assert_eq!(
        FromRelay::decode(&recv(&mut g2).await).unwrap(),
        FromRelay::Data {
            peer: p,
            payload: b"up"
        }
    );
    send(&mut g2, data(p, b"down")).await;
    assert_eq!(recv(&mut d).await, b"down");
    let (mut d2, _) = r.device(&mut g2, T1).await;
    quiet(&mut d2, Duration::from_millis(100)).await;

    // (c) The same epoch again (the gateway retrying) replaces it.
    let mut g3 = r.gateway_at(20, &[T1]).await;
    assert_eq!(closed(&mut g2).await, close::REPLACED);
    assert_eq!(closed(&mut d).await, close::GATEWAY_OFFLINE);
    assert_eq!(closed(&mut d2).await, close::GATEWAY_OFFLINE);
    let (mut d3, p3) = r.device(&mut g3, T1).await;
    send(&mut g3, data(p3, b"hi")).await;
    assert_eq!(recv(&mut d3).await, b"hi");

    // Epochs are not kept once the gateway leaves (its devices are closed by then): any epoch is
    // admitted again.
    g3.close(None).await.unwrap();
    assert_eq!(closed(&mut d3).await, close::GATEWAY_OFFLINE);
    let mut g4 = r.gateway_hello(&r.key, 1).await;
    assert_eq!(recv(&mut g4).await, HELLO_OK);
    r.stop().await.unwrap();
}

#[tokio::test]
async fn oversized_and_text_messages_close_the_sender() {
    let r = Relay::start(|_| {}).await;
    let mut g = r.gateway(&[T1, T2]).await;
    let (mut d1, p1) = r.device(&mut g, T1).await;
    send(&mut d1, vec![0; MAX_PAYLOAD]).await;
    assert_eq!(recv(&mut g).await.len(), 5 + MAX_PAYLOAD);
    send(&mut d1, vec![0; MAX_PAYLOAD + 1]).await;
    assert_eq!(closed(&mut d1).await, close::TOO_LARGE);
    assert_eq!(
        FromRelay::decode(&recv(&mut g).await).unwrap(),
        FromRelay::Closed { peer: p1 }
    );
    // Far beyond the limit (tungstenite's own cap) is TOO_LARGE too.
    let (mut d2, p2) = r.device(&mut g, T2).await;
    let _ = d2
        .send(Message::Binary(vec![0; 4 * MAX_PAYLOAD].into()))
        .await;
    assert_eq!(
        FromRelay::decode(&recv(&mut g).await).unwrap(),
        FromRelay::Closed { peer: p2 }
    );

    // A gateway text message after its hello.
    let mut g2 = r.gateway(&[]).await;
    assert_eq!(closed(&mut g).await, close::REPLACED);
    g2.send(Message::text("hi")).await.unwrap();
    assert_eq!(closed(&mut g2).await, close::PROTOCOL);
    // An undecodable gateway message.
    let mut g3 = r.gateway(&[]).await;
    send(&mut g3, vec![0x7f]).await;
    assert_eq!(closed(&mut g3).await, close::PROTOCOL);
    r.stop().await.unwrap();
}

#[tokio::test]
async fn message_floods_are_rate_limited() {
    let r = Relay::start(|c| {
        c.device_rate = RateLimit {
            messages_per_sec: 10,
            messages_burst: 5,
            ..RateLimit::DEVICE
        };
    })
    .await;
    let mut g = r.gateway(&[T1]).await;
    let (mut d, p) = r.device(&mut g, T1).await;
    for _ in 0..20 {
        if d.send(Message::Binary(vec![1; 10].into())).await.is_err() {
            break;
        }
    }
    assert_eq!(closed(&mut d).await, close::RATE_LIMITED);
    let mut forwarded = 0;
    loop {
        match FromRelay::decode(&recv(&mut g).await).unwrap() {
            FromRelay::Data { peer, .. } => {
                assert_eq!(peer, p);
                forwarded += 1;
            }
            FromRelay::Closed { peer } => {
                assert_eq!(peer, p);
                break;
            }
            other => panic!("{other:?}"),
        }
    }
    // The hello spent one message token.
    assert!(forwarded <= 5, "{forwarded} messages passed a burst of 5");

    // Byte budget.
    let r2 = Relay::start(|c| {
        c.device_rate = RateLimit {
            bytes_per_sec: 1000,
            bytes_burst: 100_000,
            ..RateLimit::DEVICE
        };
    })
    .await;
    let mut g = r2.gateway(&[T1]).await;
    let (mut d, _) = r2.device(&mut g, T1).await;
    for _ in 0..3 {
        if d.send(Message::Binary(vec![1; 60_000].into()))
            .await
            .is_err()
        {
            break;
        }
    }
    assert_eq!(closed(&mut d).await, close::RATE_LIMITED);
    r.stop().await.unwrap();
    r2.stop().await.unwrap();
}

/// A device that keeps writing large messages after the relay closed it still receives the close
/// frame: the relay drains its unread input instead of dropping the socket with data pending (which
/// makes the OS answer with a reset that can destroy the close frame in flight, as on Windows).
#[tokio::test]
async fn a_device_that_keeps_flooding_still_gets_the_close_code() {
    let r = Relay::start(|c| {
        c.device_rate = RateLimit {
            messages_per_sec: 1,
            messages_burst: 3,
            ..RateLimit::DEVICE
        };
    })
    .await;
    let mut g = r.gateway(&[T1]).await;
    for (first, code) in [
        (vec![1; 16 * 1024], close::RATE_LIMITED),
        (vec![1; MAX_PAYLOAD + 1], close::TOO_LARGE),
    ] {
        let (d, p) = r.device(&mut g, T1).await;
        let (mut tx, mut rx) = d.split();
        // Writes until the client notices the close (at most 64 MiB): unread input is
        // guaranteed to be pending at the relay when it closes.
        let flood = tokio::spawn(async move {
            tx.send(Message::Binary(first.into())).await?;
            for _ in 0..4096 {
                tx.send(Message::Binary(vec![2; 16 * 1024].into())).await?;
            }
            Ok::<_, tungstenite::Error>(())
        });
        let got = loop {
            match timeout(WAIT, rx.next()).await.expect("close in time") {
                Some(Ok(Message::Close(frame))) => break frame.map_or(0, |f| u16::from(f.code)),
                Some(Ok(_)) => {}
                other => panic!("expected a close frame, got {other:?}"),
            }
        };
        assert_eq!(got, code);
        // The close handshake completes cleanly: no reset on either half.
        match timeout(WAIT, rx.next()).await.expect("end in time") {
            None | Some(Err(tungstenite::Error::ConnectionClosed)) => {}
            other => panic!("expected a clean end, got {other:?}"),
        }
        match flood.await.unwrap() {
            Err(tungstenite::Error::Io(e)) => panic!("the flood ended with a reset: {e}"),
            Err(_) => {}
            Ok(()) => panic!("64 MiB flood was never closed"),
        }
        // Discarded input is not forwarded: at most the burst reached the gateway.
        let mut forwarded = 0;
        loop {
            match FromRelay::decode(&recv(&mut g).await).unwrap() {
                FromRelay::Data { peer, .. } => {
                    assert_eq!(peer, p);
                    forwarded += 1;
                }
                FromRelay::Closed { peer } => {
                    assert_eq!(peer, p);
                    break;
                }
                other => panic!("{other:?}"),
            }
        }
        assert!(forwarded <= 3, "{forwarded} messages passed a burst of 3");
    }
    r.stop().await.unwrap();
}

/// A client with a tiny receive buffer (it fills up fast when the client stops reading).
async fn small_buffer_ws(r: &Relay, path: &str) -> Ws {
    let socket = TcpSocket::new_v4().unwrap();
    socket.set_recv_buffer_size(4096).unwrap();
    let tcp = socket.connect(r.addr).await.unwrap();
    tokio_tungstenite::client_async(format!("ws://{}{path}", r.addr), tcp)
        .await
        .unwrap()
        .0
}

/// A device that will not read.
async fn stalled_device(r: &Relay, gateway: &mut Ws, token: Token) -> (Ws, PeerId) {
    let mut ws = small_buffer_ws(r, DEVICE_PATH).await;
    send(
        &mut ws,
        Hello::Device {
            room: r.key.room,
            token,
        }
        .encode(),
    )
    .await;
    assert_eq!(recv(&mut ws).await, HELLO_OK);
    match FromRelay::decode(&recv(gateway).await).unwrap() {
        FromRelay::Open { peer, .. } => (ws, peer),
        other => panic!("{other:?}"),
    }
}

/// Per-device inbound limits high enough that only the queue bounds matter.
const UNLIMITED: RateLimit = RateLimit {
    bytes_per_sec: 1 << 30,
    bytes_burst: 1 << 30,
    messages_per_sec: 1 << 20,
    messages_burst: 1 << 20,
};

/// An admitted gateway with a tiny receive buffer (it stalls as soon as the test stops reading).
async fn stalled_gateway(r: &Relay, tokens: &[Token]) -> Ws {
    let mut g = small_buffer_ws(r, GATEWAY_PATH).await;
    send(
        &mut g,
        Hello::Gateway {
            room: r.key.room,
            secret: r.key.secret,
            epoch: 0,
        }
        .encode(),
    )
    .await;
    assert_eq!(recv(&mut g).await, HELLO_OK);
    set_tokens(&mut g, tokens).await;
    r.tokens_applied().await;
    g
}

/// Sends maximal payloads until the relay closes `d`; returns the close code.
async fn flood_until_closed(d: &mut Ws) -> u16 {
    let chunk = vec![3u8; MAX_PAYLOAD];
    for _ in 0..1500 {
        if d.send(Message::Binary(chunk.clone().into())).await.is_err() {
            break;
        }
        if let Ok(Some(msg)) = timeout(Duration::ZERO, d.next()).await {
            match msg {
                Ok(Message::Close(f)) => return f.map_or(0, |f| u16::from(f.code)),
                other => panic!("{other:?}"),
            }
        }
    }
    closed(d).await
}

/// A gateway that stopped reading: device data fills at most 3/4 of its queue (one token's at
/// most 1/4), then the sending device is closed `SLOW_CONSUMER` and the gateway stays. Only
/// `Open`/`Closed` events beyond the headroom (a gateway that really is gone) drop it.
#[tokio::test]
async fn a_flood_toward_a_stalled_gateway_closes_the_device_not_the_gateway() {
    let r = Relay::start(|c| {
        c.queue_messages = 16;
        c.device_rate = UNLIMITED;
        // One token reconnects below: only the room's admission rate applies.
        c.token_admissions = c.device_admissions;
    })
    .await;
    let mut g = stalled_gateway(&r, &[T1]).await;
    let (mut d, _) = r.device(&mut g, T1).await;
    assert_eq!(flood_until_closed(&mut d).await, close::SLOW_CONSUMER);

    // The gateway is still the room's gateway: devices are admitted. Each admission and leave
    // queues an event in the headroom; once that is spent too, the gateway is dropped.
    let mut dropped = false;
    for _ in 0..16 {
        let mut d = r.device_hello(T1).await;
        match timeout(WAIT, d.next()).await.expect("hello answer") {
            Some(Ok(Message::Binary(b))) => {
                assert_eq!(b.to_vec(), HELLO_OK);
                d.close(None).await.unwrap();
                finish(d).await;
            }
            Some(Ok(Message::Close(Some(f)))) => {
                assert_eq!(u16::from(f.code), close::GATEWAY_OFFLINE);
                dropped = true;
                break;
            }
            other => panic!("{other:?}"),
        }
    }
    assert!(dropped, "a gateway that never reads is dropped eventually");
    assert_eq!(closed(&mut g).await, close::SLOW_CONSUMER);
    r.stop().await.unwrap();
}

/// The review's reproduction: one device writes 1000 one-byte frames at once toward a gateway
/// that keeps reading (production limits). The gateway is never dropped.
#[tokio::test]
async fn a_device_flood_never_drops_a_reading_gateway() {
    let r = Relay::start(|_| {}).await;
    let mut g = r.gateway(&[T1, T2]).await;
    let (mut d, p) = r.device(&mut g, T1).await;
    for _ in 0..1000 {
        d.feed(Message::Binary(vec![1].into())).await.unwrap();
    }
    d.flush().await.unwrap();

    // The gateway reads the flood (whatever part was forwarded) and still serves a new device.
    let mut d2 = r.device_hello(T2).await;
    let p2 = loop {
        match FromRelay::decode(&recv(&mut g).await).unwrap() {
            FromRelay::Data { peer, payload } => {
                assert_eq!((peer, payload), (p, &[1u8][..]));
            }
            FromRelay::Closed { peer } => assert_eq!(peer, p),
            FromRelay::Open { peer, .. } => break peer,
        }
    };
    assert_eq!(recv(&mut d2).await, HELLO_OK);
    send(&mut g, data(p2, b"still here")).await;
    assert_eq!(recv(&mut d2).await, b"still here");
    send(&mut d2, b"me too".to_vec()).await;
    loop {
        match FromRelay::decode(&recv(&mut g).await).unwrap() {
            FromRelay::Data { peer, payload } if peer == p2 => {
                assert_eq!(payload, b"me too");
                break;
            }
            FromRelay::Data { peer, .. } | FromRelay::Closed { peer } => assert_eq!(peer, p),
            other => panic!("{other:?}"),
        }
    }
    r.stop().await.unwrap();
}

#[tokio::test]
async fn a_token_admits_at_most_two_concurrent_connections() {
    let r = Relay::start(|_| {}).await;
    let mut g = r.gateway(&[T1, T2]).await;
    let (mut d1, p1) = r.device(&mut g, T1).await;
    let (_d2, _) = r.device(&mut g, T1).await;
    let mut d3 = r.device_hello(T1).await;
    assert_eq!(closed(&mut d3).await, close::RATE_LIMITED);
    // Other tokens are unaffected.
    let (_e1, _) = r.device(&mut g, T2).await;
    // A leaving connection frees its token's slot.
    d1.close(None).await.unwrap();
    assert_eq!(
        FromRelay::decode(&recv(&mut g).await).unwrap(),
        FromRelay::Closed { peer: p1 }
    );
    let (_d4, _) = r.device(&mut g, T1).await;
    r.stop().await.unwrap();
}

/// One token holder flooding a gateway that stopped reading fills at most a quarter of its queue
/// (counted per token, across reconnects) and is closed `SLOW_CONSUMER`; another device sending
/// afterwards is not, and its data reaches the gateway once it reads again.
#[tokio::test]
async fn a_flooding_token_cannot_get_another_device_closed() {
    let r = Relay::start(|c| {
        c.queue_messages = 16;
        c.device_rate = UNLIMITED;
    })
    .await;
    let mut g = stalled_gateway(&r, &[T1, T2]).await;
    let (mut calm, p2) = r.device(&mut g, T2).await;
    let (mut flood, p1) = r.device(&mut g, T1).await;
    assert_eq!(flood_until_closed(&mut flood).await, close::SLOW_CONSUMER);
    // Reconnecting does not reset the token's share: its queued data still counts.
    let mut again = r.device_hello(T1).await;
    assert_eq!(recv(&mut again).await, HELLO_OK);
    send(&mut again, vec![3; MAX_PAYLOAD]).await;
    assert_eq!(closed(&mut again).await, close::SLOW_CONSUMER);

    // The other device still has its share of the (stalled) gateway's queue.
    send(&mut calm, b"hello".to_vec()).await;
    quiet(&mut calm, Duration::from_millis(300)).await;
    loop {
        match FromRelay::decode(&recv(&mut g).await).unwrap() {
            FromRelay::Data { peer, payload } if peer == p2 => {
                assert_eq!(payload, b"hello");
                break;
            }
            FromRelay::Data { peer, .. } => assert_eq!(peer, p1, "only the flood is queued"),
            FromRelay::Closed { peer } => assert_ne!(peer, p2, "the calm device was closed"),
            FromRelay::Open { .. } => {}
        }
    }
    send(&mut g, data(p2, b"still here")).await;
    assert_eq!(recv(&mut calm).await, b"still here");
    r.stop().await.unwrap();
}

/// Device admissions per room are rate limited (10/s, burst 20) before `Open` is queued, so
/// connect/leave churn cannot spend a gateway's queue headroom: the excess gets `RATE_LIMITED`
/// and a reading gateway is never dropped.
#[tokio::test]
async fn admission_churn_is_rate_limited_and_never_drops_a_reading_gateway() {
    let r = Relay::start(|_| {}).await;
    // Eight churning tokens: their own buckets (burst 4 each) together exceed the room's.
    let churners: Vec<Token> = (10..18).map(|i| [i; 32]).collect();
    let mut tokens = churners.clone();
    tokens.push(T2);
    let mut g = r.gateway(&tokens).await;
    // The gateway keeps reading throughout; any close or error fails the test.
    let (stop, mut stopped) = oneshot::channel::<()>();
    let reader = tokio::spawn(async move {
        let mut events = 0usize;
        loop {
            tokio::select! {
                _ = &mut stopped => return (g, events),
                msg = g.next() => match msg {
                    Some(Ok(Message::Binary(b))) => {
                        assert!(matches!(
                            FromRelay::decode(&b).unwrap(),
                            FromRelay::Open { .. } | FromRelay::Closed { .. }
                        ));
                        events += 1;
                    }
                    Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                    other => panic!("gateway: {other:?}"),
                },
            }
        }
    });

    let started = Instant::now();
    let (mut admitted, mut limited) = (0u64, 0u64);
    for i in 0..60 {
        let mut d = r.device_hello(churners[i % churners.len()]).await;
        match timeout(WAIT, d.next()).await.expect("hello answer") {
            Some(Ok(Message::Binary(b))) => {
                assert_eq!(b.to_vec(), HELLO_OK);
                admitted += 1;
                d.close(None).await.unwrap();
            }
            Some(Ok(Message::Close(Some(f)))) => {
                assert_eq!(u16::from(f.code), close::RATE_LIMITED);
                limited += 1;
            }
            other => panic!("{other:?}"),
        }
        finish(d).await;
    }
    let elapsed = started.elapsed().as_secs_f64();
    assert!(
        limited > 0,
        "60 admissions in {elapsed:.2}s were not limited"
    );
    assert!(
        admitted as f64 <= 20.0 + 10.0 * elapsed + 1.0,
        "{admitted} admitted in {elapsed:.2}s"
    );

    // The gateway saw every admission and leave, and still serves a device once tokens refill.
    sleep(Duration::from_millis(250)).await;
    stop.send(()).unwrap();
    let (mut g, _) = reader.await.unwrap();
    let mut d = r.device_hello(T2).await;
    assert_eq!(recv(&mut d).await, HELLO_OK);
    let p = loop {
        match FromRelay::decode(&recv(&mut g).await).unwrap() {
            FromRelay::Open { peer, .. } => break peer,
            FromRelay::Closed { .. } => {}
            other => panic!("{other:?}"),
        }
    };
    send(&mut g, data(p, b"ok")).await;
    assert_eq!(recv(&mut d).await, b"ok");
    r.stop().await.unwrap();
}

/// Each token has its own admission bucket (2/s, burst 4), checked before the room's: a token
/// holder churning connections gets `RATE_LIMITED` without spending the room's admissions, so
/// another token's device (a reconnecting phone, a new pairing) is still admitted right away.
#[tokio::test]
async fn a_churning_token_cannot_starve_another_tokens_admission() {
    let r = Relay::start(|_| {}).await;
    let mut g = r.gateway(&[T1, T2]).await;
    let (mut admitted, mut limited) = (0, 0);
    for _ in 0..30 {
        let mut d = r.device_hello(T1).await;
        match timeout(WAIT, d.next()).await.expect("hello answer") {
            Some(Ok(Message::Binary(b))) => {
                assert_eq!(b.to_vec(), HELLO_OK);
                admitted += 1;
                d.close(None).await.unwrap();
            }
            Some(Ok(Message::Close(Some(f)))) => {
                assert_eq!(u16::from(f.code), close::RATE_LIMITED);
                limited += 1;
            }
            other => panic!("{other:?}"),
        }
        finish(d).await;
    }
    assert!(
        admitted >= 4 && limited > 0,
        "{admitted} admitted, {limited} limited"
    );
    // Right away, while the churning token is limited: another token is admitted.
    let mut d = r.device_hello(T2).await;
    assert_eq!(recv(&mut d).await, HELLO_OK);
    let t2 = relay::hash(&T2);
    let p = loop {
        match FromRelay::decode(&recv(&mut g).await).unwrap() {
            FromRelay::Open { peer, token } if token == t2 => break peer,
            FromRelay::Open { .. } | FromRelay::Closed { .. } => {}
            other => panic!("{other:?}"),
        }
    };
    send(&mut g, data(p, b"paired")).await;
    assert_eq!(recv(&mut d).await, b"paired");
    r.stop().await.unwrap();
}

#[tokio::test]
async fn a_slow_device_is_closed_without_stalling_another() {
    let r = Relay::start(|c| {
        c.queue_messages = 16;
        c.gateway_rate = RateLimit {
            bytes_per_sec: 1 << 30,
            bytes_burst: 1 << 30,
            messages_per_sec: 1 << 20,
            messages_burst: 1 << 20,
        };
    })
    .await;
    let mut g = r.gateway(&[T1, T2]).await;
    let (mut slow, ps) = stalled_device(&r, &mut g, T1).await;
    let (fast, pf) = r.device(&mut g, T2).await;

    let fast_reader = tokio::spawn(async move {
        let mut fast = fast;
        for i in 0..50u32 {
            assert_eq!(recv(&mut fast).await, i.to_le_bytes());
        }
        fast
    });
    let chunk = vec![7u8; MAX_PAYLOAD];
    let mut sent_fast = 0u32;
    let started = Instant::now();
    for i in 0..1500 {
        g.feed(Message::Binary(data(ps, &chunk).into()))
            .await
            .unwrap();
        if i % 30 == 0 && sent_fast < 50 {
            g.feed(Message::Binary(data(pf, &sent_fast.to_le_bytes()).into()))
                .await
                .unwrap();
            sent_fast += 1;
        }
    }
    while sent_fast < 50 {
        g.feed(Message::Binary(data(pf, &sent_fast.to_le_bytes()).into()))
            .await
            .unwrap();
        sent_fast += 1;
    }
    g.flush().await.unwrap();
    let _fast = timeout(WAIT, fast_reader)
        .await
        .expect("the fast device is not stalled")
        .unwrap();
    assert!(started.elapsed() < WAIT);

    // The gateway learns that the slow device left, and the device gets SLOW_CONSUMER once it reads.
    loop {
        if let FromRelay::Closed { peer } = FromRelay::decode(&recv(&mut g).await).unwrap() {
            assert_eq!(peer, ps);
            break;
        }
    }
    assert_eq!(closed(&mut slow).await, close::SLOW_CONSUMER);
    r.stop().await.unwrap();
}

#[tokio::test]
async fn connection_and_device_caps() {
    let r = Relay::start(|c| {
        c.max_connections_per_ip = 3;
        c.max_devices_per_room = 1;
    })
    .await;
    let mut g = r.gateway(&[T1, T2]).await;
    let (_d1, _) = r.device(&mut g, T1).await;
    // Device cap per room.
    let mut d2 = r.device_hello(T2).await;
    assert_eq!(closed(&mut d2).await, close::RATE_LIMITED);
    drop(d2);
    // Wait until the rejected connection released its slot.
    sleep(Duration::from_millis(200)).await;
    let _third = r.ws(DEVICE_PATH).await;
    // Per-IP cap: the socket is closed right after accept, before any HTTP.
    let tcp = TcpStream::connect(r.addr).await.unwrap();
    match tokio_tungstenite::client_async(format!("ws://{}{DEVICE_PATH}", r.addr), tcp).await {
        Err(tungstenite::Error::Http(resp)) => panic!("expected a closed socket, got {resp:?}"),
        Err(_) => {}
        Ok(_) => panic!("connected over the per-IP cap"),
    }
    r.stop().await.unwrap();
}

/// The socket is closed by the relay (EOF or reset) within [`WAIT`].
async fn refused<S: AsyncRead + Unpin>(mut stream: S) {
    let mut buf = [0u8; 64];
    match timeout(WAIT, stream.read(&mut buf)).await {
        Ok(Ok(0) | Err(_)) => {}
        Ok(Ok(n)) => panic!("expected a closed socket, read {n} bytes"),
        Err(_) => panic!("socket still open after {WAIT:?}"),
    }
}

/// The relay keeps the socket open (nothing to read, no close) for `wait`.
async fn held_open(stream: &mut TcpStream, wait: Duration) {
    let mut buf = [0u8; 64];
    match timeout(wait, stream.read(&mut buf)).await {
        Err(_) => {}
        Ok(r) => panic!("expected an open, silent socket, got {r:?}"),
    }
}

/// Retries `connect` until it succeeds within [`WAIT`] (closed sockets free their slot as soon as
/// the relay notices the close).
async fn eventually<T, F: Future<Output = Option<T>>>(mut connect: impl FnMut() -> F) -> T {
    let deadline = Instant::now() + WAIT;
    loop {
        if let Some(t) = connect().await {
            return t;
        }
        assert!(Instant::now() < deadline, "no connection within {WAIT:?}");
        sleep(Duration::from_millis(50)).await;
    }
}

async fn try_gateway(r: &Relay) -> Option<Ws> {
    let tcp = TcpStream::connect(r.addr).await.ok()?;
    let (mut ws, _) =
        tokio_tungstenite::client_async(format!("ws://{}{GATEWAY_PATH}", r.addr), tcp)
            .await
            .ok()?;
    let hello = Hello::Gateway {
        room: r.key.room,
        secret: r.key.secret,
        epoch: 0,
    };
    ws.send(Message::Binary(hello.encode().into())).await.ok()?;
    Some(ws)
}

#[tokio::test]
async fn stalled_plain_sockets_count_against_the_caps() {
    for (total, per_ip) in [(3, 32), (32, 3)] {
        let r = Relay::start(|c| {
            c.max_connections = total;
            c.max_connections_per_ip = per_ip;
        })
        .await;
        // Sockets that never send a request hold their slots…
        let mut stalled = Vec::new();
        for _ in 0..3 {
            stalled.push(TcpStream::connect(r.addr).await.unwrap());
        }
        for s in &mut stalled {
            held_open(s, Duration::from_millis(100)).await;
        }
        // …so further connections are closed at accept.
        for _ in 0..3 {
            refused(TcpStream::connect(r.addr).await.unwrap()).await;
        }
        drop(stalled);
        let mut g = eventually(|| try_gateway(&r)).await;
        assert_eq!(recv(&mut g).await, HELLO_OK);
        r.stop().await.unwrap();
    }
}

#[tokio::test]
async fn connections_that_do_not_upgrade_in_time_are_closed() {
    let r = Relay::start(|c| c.request_timeout = Duration::from_millis(300)).await;
    let started = Instant::now();
    let silent = TcpStream::connect(r.addr).await.unwrap();
    let mut partial = TcpStream::connect(r.addr).await.unwrap();
    partial
        .write_all(b"GET /healthz HTTP/1.1\r\nHost: x\r\n")
        .await
        .unwrap();
    // A keep-alive health check: answered, then closed at the deadline.
    let mut keep_alive = TcpStream::connect(r.addr).await.unwrap();
    keep_alive
        .write_all(b"GET /healthz HTTP/1.1\r\nHost: x\r\n\r\n")
        .await
        .unwrap();
    let mut g = r.gateway(&[T1]).await;
    refused(silent).await;
    refused(partial).await;
    let mut resp = Vec::new();
    timeout(WAIT, keep_alive.read_to_end(&mut resp))
        .await
        .expect("keep-alive closed at the deadline")
        .unwrap();
    assert!(resp.starts_with(b"HTTP/1.1 200"), "{resp:?}");
    assert!(started.elapsed() >= Duration::from_millis(300));
    // The WebSocket upgraded in time and outlives the deadline.
    sleep(Duration::from_millis(500)).await;
    let (mut d, p) = r.device(&mut g, T1).await;
    send(&mut d, b"still here".to_vec()).await;
    assert_eq!(
        FromRelay::decode(&recv(&mut g).await).unwrap(),
        FromRelay::Data {
            peer: p,
            payload: b"still here"
        }
    );
    r.stop().await.unwrap();
}

#[tokio::test]
async fn hellos_and_pre_hello_messages_are_rate_limited() {
    let r = Relay::start(|c| {
        c.device_rate = RateLimit {
            messages_per_sec: 1,
            messages_burst: 3,
            ..RateLimit::DEVICE
        };
        c.gateway_rate = RateLimit {
            bytes_per_sec: 1,
            bytes_burst: 10,
            ..RateLimit::GATEWAY
        };
    })
    .await;
    // Pings before the hello spend the message budget.
    let mut d = r.ws(DEVICE_PATH).await;
    for _ in 0..5 {
        if d.send(Message::Ping(Vec::new().into())).await.is_err() {
            break;
        }
    }
    assert_eq!(closed(&mut d).await, close::RATE_LIMITED);
    // A hello over the byte budget is refused before admission.
    let mut g = r.gateway_with(&r.key).await;
    assert_eq!(closed(&mut g).await, close::RATE_LIMITED);
    r.stop().await.unwrap();
}

#[tokio::test]
async fn an_abrupt_device_disconnect_reports_closed_once() {
    let r = Relay::start(|_| {}).await;
    let mut g = r.gateway(&[T1]).await;
    let (d, p) = r.device(&mut g, T1).await;
    // No close frame: the socket just goes away (a client process that exited).
    drop(d);
    assert_eq!(
        FromRelay::decode(&recv(&mut g).await).unwrap(),
        FromRelay::Closed { peer: p }
    );
    quiet(&mut g, Duration::from_millis(300)).await;
    r.stop().await.unwrap();
}

#[tokio::test]
async fn forwarded_for_keys_the_ip_limit_only_when_trusted() {
    async fn connect(r: &Relay, client: &str) -> Result<Ws, tungstenite::Error> {
        let mut req = format!("ws://{}{DEVICE_PATH}", r.addr)
            .into_client_request()
            .unwrap();
        req.headers_mut().insert(
            "x-forwarded-for",
            format!("203.0.113.9, {client}").parse().unwrap(),
        );
        let tcp = TcpStream::connect(r.addr).await.unwrap();
        tokio_tungstenite::client_async(req, tcp)
            .await
            .map(|(ws, _)| ws)
    }
    let trusted = Relay::start(|c| {
        c.max_connections_per_ip = 1;
        c.trust_forwarded_for = true;
    })
    .await;
    let _a = connect(&trusted, "198.51.100.1").await.unwrap();
    let _b = connect(&trusted, "198.51.100.2").await.unwrap();
    assert!(connect(&trusted, "198.51.100.2").await.is_err());

    let untrusted = Relay::start(|c| c.max_connections_per_ip = 1).await;
    let _a = connect(&untrusted, "198.51.100.1").await.unwrap();
    assert!(connect(&untrusted, "198.51.100.2").await.is_err());
    drop(_a);

    // Per-IP keys: IPv6 per /64, IPv4-mapped IPv6 as IPv4.
    let _v6 = connect(&trusted, "2001:db8::1").await.unwrap();
    assert!(connect(&trusted, "2001:db8::2").await.is_err(), "same /64");
    let _v6b = connect(&trusted, "2001:db8:0:1::1").await.unwrap();
    let _mapped = connect(&trusted, "::ffff:198.51.100.7").await.unwrap();
    assert!(
        connect(&trusted, "198.51.100.7").await.is_err(),
        "same IPv4"
    );
    trusted.stop().await.unwrap();
    untrusted.stop().await.unwrap();
}

#[tokio::test]
async fn gateway_hellos_reload_rooms_only_for_unknown_rooms_at_most_once_a_second() {
    let r = Relay::start(|_| {}).await;
    // A room in the loaded table is admitted without re-reading rooms.json (its removal on disk
    // is noticed by the next reload).
    let mut first = r.gateway_with(&r.key).await;
    assert_eq!(
        recv(&mut first).await,
        HELLO_OK,
        "the relay loaded its table"
    );
    assert!(rooms::remove(r.state(), &relay::room_id(&r.key.room)).unwrap());
    let mut g = r.gateway_with(&r.key).await;
    assert_eq!(recv(&mut g).await, HELLO_OK);
    assert_eq!(closed(&mut first).await, close::REPLACED);

    // An unknown room triggers a reload…
    let started = Instant::now();
    let unknown = RoomKey {
        room: [7; 16],
        secret: [7; 32],
    };
    let mut u = r.gateway_with(&unknown).await;
    assert_eq!(closed(&mut u).await, close::UNAUTHORIZED);
    assert_eq!(
        closed(&mut g).await,
        close::UNAUTHORIZED,
        "the reload saw the removal"
    );
    // …and the next one waits a second; hellos arriving meanwhile share it.
    let (key, _) = rooms::enroll(r.state(), "new").unwrap();
    let mut burst = Vec::new();
    for _ in 0..5 {
        burst.push(r.gateway_with(&unknown).await);
    }
    let mut n = r.gateway_with(&key).await;
    assert_eq!(recv(&mut n).await, HELLO_OK);
    assert!(started.elapsed() >= Duration::from_millis(900));
    for mut u in burst {
        assert_eq!(closed(&mut u).await, close::UNAUTHORIZED);
    }
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "one shared reload, not one per hello"
    );
    r.stop().await.unwrap();
}

#[tokio::test]
async fn removing_a_room_closes_its_connections_and_corrupt_files_are_ignored() {
    let r = Relay::start(|c| c.rooms_reload_interval = Duration::from_millis(100)).await;
    let mut g = r.gateway(&[T1]).await;
    let (mut d, p) = r.device(&mut g, T1).await;

    // A corrupt file keeps the previous table: traffic and new hellos still work.
    let path = rooms::rooms_path(r.state());
    let good = std::fs::read(&path).unwrap();
    std::fs::write(&path, b"{ not json").unwrap();
    sleep(Duration::from_millis(300)).await;
    send(&mut d, b"ping".to_vec()).await;
    assert_eq!(
        FromRelay::decode(&recv(&mut g).await).unwrap(),
        FromRelay::Data {
            peer: p,
            payload: b"ping"
        }
    );
    let mut g2 = r.gateway(&[T1]).await;
    assert_eq!(closed(&mut g).await, close::REPLACED);
    assert_eq!(closed(&mut d).await, close::GATEWAY_OFFLINE);
    std::fs::write(&path, good).unwrap();
    let (mut d, _) = r.device(&mut g2, T1).await;

    let id = relay::room_id(&r.key.room);
    assert!(rooms::remove(r.state(), &id).unwrap());
    assert_eq!(closed(&mut g2).await, close::UNAUTHORIZED);
    assert_eq!(closed(&mut d).await, close::UNAUTHORIZED);
    let mut g3 = r.gateway_with(&r.key).await;
    assert_eq!(closed(&mut g3).await, close::UNAUTHORIZED);
    r.stop().await.unwrap();
}

#[tokio::test]
async fn pings_keep_connections_alive_and_silence_closes_them() {
    let r = Relay::start(|c| {
        c.ping_interval = Duration::from_millis(100);
        c.idle_timeout = Duration::from_millis(400);
    })
    .await;
    let mut g = r.gateway(&[T1]).await;
    let (mut d, _) = r.device(&mut g, T1).await;
    // Both clients keep reading, so tungstenite answers the relay's pings.
    tokio::join!(
        quiet(&mut d, Duration::from_millis(1200)),
        quiet(&mut g, Duration::from_millis(1200))
    );
    send(&mut d, b"alive".to_vec()).await;
    assert!(matches!(
        FromRelay::decode(&recv(&mut g).await).unwrap(),
        FromRelay::Data { .. }
    ));
    r.stop().await.unwrap();

    let r = Relay::start(|c| {
        c.ping_interval = Duration::from_secs(60);
        c.idle_timeout = Duration::from_millis(300);
    })
    .await;
    let mut g = r.gateway(&[]).await;
    assert_eq!(closed(&mut g).await, close::NORMAL);
    r.stop().await.unwrap();
}

async fn http_get<S: AsyncRead + AsyncWrite + Unpin>(mut stream: S, path: &str) -> String {
    stream
        .write_all(
            format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .await
        .unwrap();
    let mut out = String::new();
    timeout(WAIT, stream.read_to_string(&mut out))
        .await
        .unwrap()
        .unwrap();
    out
}

#[tokio::test]
async fn healthz_answers_ok() {
    let r = Relay::start(|_| {}).await;
    let resp = http_get(TcpStream::connect(r.addr).await.unwrap(), "/healthz").await;
    assert!(resp.starts_with("HTTP/1.1 200"), "{resp}");
    assert!(resp.ends_with("\r\n\r\nok"), "{resp}");
    r.stop().await.unwrap();
}

#[tokio::test]
async fn shutdown_closes_every_socket_going_away() {
    let r = Relay::start(|_| {}).await;
    let mut g = r.gateway(&[T1]).await;
    let (mut d, _) = r.device(&mut g, T1).await;
    let mut pending = r.ws(DEVICE_PATH).await;
    let addr = r.addr;
    let started = Instant::now();
    let stopping = tokio::spawn(r.stop());
    assert_eq!(closed(&mut g).await, close::GOING_AWAY);
    assert_eq!(closed(&mut d).await, close::GOING_AWAY);
    assert_eq!(closed(&mut pending).await, close::GOING_AWAY);
    stopping.await.unwrap().unwrap();
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(TcpStream::connect(addr).await.is_err(), "listener closed");
}

#[tokio::test]
async fn in_process_tls() {
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let tls = deck_relay::load_tls(&fixtures.join("cert.pem"), &fixtures.join("key.pem")).unwrap();
    let r = Relay::start(|c| c.tls = Some(tls)).await;

    use rustls::pki_types::pem::PemObject as _;
    let mut roots = rustls::RootCertStore::empty();
    for cert in rustls::pki_types::CertificateDer::pem_file_iter(fixtures.join("ca.pem")).unwrap() {
        roots.add(cert.unwrap()).unwrap();
    }
    let client = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_root_certificates(roots)
    .with_no_client_auth();
    let connector = tokio_rustls::TlsConnector::from(Arc::new(client));
    let connect = || async {
        let tcp = TcpStream::connect(r.addr).await.unwrap();
        connector
            .connect("localhost".try_into().unwrap(), tcp)
            .await
            .unwrap()
    };

    let resp = http_get(connect().await, "/healthz").await;
    assert!(resp.starts_with("HTTP/1.1 200"), "{resp}");

    let (mut g, _) =
        tokio_tungstenite::client_async(format!("wss://localhost{GATEWAY_PATH}"), connect().await)
            .await
            .unwrap();
    send(
        &mut g,
        Hello::Gateway {
            room: r.key.room,
            secret: r.key.secret,
            epoch: 0,
        }
        .encode(),
    )
    .await;
    assert_eq!(recv(&mut g).await, HELLO_OK);

    // A plain client is refused by the TLS listener.
    let tcp = TcpStream::connect(r.addr).await.unwrap();
    assert!(
        tokio_tungstenite::client_async(format!("ws://localhost{GATEWAY_PATH}"), tcp)
            .await
            .is_err()
    );
    let stopping = tokio::spawn(r.stop());
    assert_eq!(closed(&mut g).await, close::GOING_AWAY);
    stopping.await.unwrap().unwrap();
}

#[tokio::test]
async fn stalled_tls_handshakes_count_against_the_caps() {
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let tls = deck_relay::load_tls(&fixtures.join("cert.pem"), &fixtures.join("key.pem")).unwrap();
    let r = Relay::start(|c| {
        c.tls = Some(tls);
        c.max_connections = 2;
    })
    .await;

    use rustls::pki_types::pem::PemObject as _;
    let mut roots = rustls::RootCertStore::empty();
    for cert in rustls::pki_types::CertificateDer::pem_file_iter(fixtures.join("ca.pem")).unwrap() {
        roots.add(cert.unwrap()).unwrap();
    }
    let client = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_root_certificates(roots)
    .with_no_client_auth();
    let connector = tokio_rustls::TlsConnector::from(Arc::new(client));
    let health = || async {
        let tcp = TcpStream::connect(r.addr).await.ok()?;
        let tls = connector
            .connect("localhost".try_into().unwrap(), tcp)
            .await
            .ok()?;
        Some(http_get(tls, "/healthz").await)
    };

    // Sockets that never start the TLS handshake hold their slots…
    let mut stalled = Vec::new();
    for _ in 0..2 {
        stalled.push(TcpStream::connect(r.addr).await.unwrap());
    }
    for s in &mut stalled {
        held_open(s, Duration::from_millis(100)).await;
    }
    // …so further sockets are closed before any handshake, and TLS clients fail.
    refused(TcpStream::connect(r.addr).await.unwrap()).await;
    assert!(health().await.is_none());
    drop(stalled);
    let resp = eventually(health).await;
    assert!(resp.starts_with("HTTP/1.1 200"), "{resp}");
    r.stop().await.unwrap();
}
