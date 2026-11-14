//! Real-socket relay-link integration: freezing a TCP path must not leave the gateway up.

use super::*;
use std::collections::VecDeque;
use std::net::{SocketAddr, TcpListener};
use std::path::Path;
use std::sync::atomic::AtomicUsize;

struct Relay {
    addr: SocketAddr,
    key: relay::RoomKey,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl Relay {
    fn start(dir: &Path) -> Self {
        let (key, _) = deck_relay::rooms::enroll(dir, "half-open test").unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let mut config = deck_relay::Config::new(dir.to_owned());
        config.ping_interval = Duration::from_millis(60);
        config.idle_timeout = Duration::from_secs(10);
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let thread = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async move {
                deck_relay::serve(
                    config,
                    tokio::net::TcpListener::from_std(listener).unwrap(),
                    async move {
                        let _ = stopped.await;
                    },
                )
                .await
                .unwrap();
            });
        });
        Self {
            addr,
            key,
            stop: Some(stop),
            thread: Some(thread),
        }
    }
}

impl Drop for Relay {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
    }
}

struct Pair {
    gateway: TcpStream,
    relay: TcpStream,
    upstream: VecDeque<u8>,
    downstream: VecDeque<u8>,
}

impl Pair {
    fn forward(&mut self, downstream_bytes: &AtomicUsize) -> io::Result<bool> {
        Ok(
            forward(&mut self.gateway, &mut self.relay, &mut self.upstream, None)?
                && forward(
                    &mut self.relay,
                    &mut self.gateway,
                    &mut self.downstream,
                    Some(downstream_bytes),
                )?,
        )
    }
}

fn forward(
    source: &mut TcpStream,
    destination: &mut TcpStream,
    pending: &mut VecDeque<u8>,
    forwarded: Option<&AtomicUsize>,
) -> io::Result<bool> {
    if pending.is_empty() {
        let mut bytes = [0; 8192];
        match source.read(&mut bytes) {
            Ok(0) => return Ok(false),
            Ok(n) => pending.extend(&bytes[..n]),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(error) => return Err(error),
        }
    }
    if !pending.is_empty() {
        match destination.write(pending.make_contiguous()) {
            Ok(0) => return Ok(false),
            Ok(n) => {
                pending.drain(..n);
                if let Some(forwarded) = forwarded {
                    forwarded.fetch_add(n, Ordering::Release);
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(error) => return Err(error),
        }
    }
    Ok(true)
}

struct Proxy {
    addr: SocketAddr,
    frozen: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    downstream_bytes: Arc<AtomicUsize>,
    thread: Option<JoinHandle<()>>,
}

impl Proxy {
    fn start(relay: SocketAddr) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let frozen = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let (frozen_worker, stop_worker) = (frozen.clone(), stop.clone());
        let downstream_bytes = Arc::new(AtomicUsize::new(0));
        let downstream_worker = downstream_bytes.clone();
        let thread = std::thread::spawn(move || {
            let mut pairs = Vec::new();
            while !stop_worker.load(Ordering::Acquire) {
                while let Ok((gateway, _)) = listener.accept() {
                    let relay = TcpStream::connect_timeout(&relay, Duration::from_secs(1)).unwrap();
                    for socket in [&gateway, &relay] {
                        socket.set_nonblocking(true).unwrap();
                        socket.set_nodelay(true).unwrap();
                    }
                    pairs.push(Pair {
                        gateway,
                        relay,
                        upstream: VecDeque::new(),
                        downstream: VecDeque::new(),
                    });
                }
                // Frozen means no reads or writes on either socket, not a disconnect.
                if !frozen_worker.load(Ordering::Acquire) {
                    pairs.retain_mut(|pair| pair.forward(&downstream_worker).unwrap_or(false));
                }
                std::thread::sleep(Duration::from_millis(2));
            }
        });
        Self {
            addr,
            frozen,
            stop,
            downstream_bytes,
            thread: Some(thread),
        }
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
    }
}

fn wait_link(events: &Receiver<RelayLink>, accepts: impl Fn(&RelayLink) -> bool) -> RelayLink {
    let until = Instant::now() + Duration::from_secs(20);
    let mut seen = Vec::new();
    loop {
        let event = events
            .recv_timeout(until.saturating_duration_since(Instant::now()))
            .unwrap_or_else(|error| panic!("link event: {error}; seen {seen:?}"));
        if accepts(&event) {
            return event;
        }
        seen.push(event);
    }
}

#[test]
fn half_open_relay_recovers_after_a_frozen_tcp_path() {
    let dir = tempfile::tempdir().unwrap();
    let relay = Relay::start(&dir.path().join("relay"));
    let proxy = Proxy::start(relay.addr);
    let timing = Timing {
        silence: Duration::from_secs(5),
        keepalive: Duration::from_millis(80),
    };
    let (events, received) = std::sync::mpsc::channel();
    let link = Link::start_with_timing(
        RelayConfig::new(&format!("ws://{}", proxy.addr), &relay.key.encode()).unwrap(),
        Keypair::generate().unwrap(),
        Arc::new(Mutex::new(Epochs::load(&dir.path().join("epoch")))),
        Vec::new(),
        None,
        Box::new(move |event| {
            if let Event::Link(state) = event {
                let _ = events.send(state);
            }
        }),
        timing,
    )
    .unwrap();
    wait_link(&received, |event| matches!(event, RelayLink::Up));
    // Observe fresh relay traffic rather than requiring uninterrupted thread scheduling
    // across a sleep. Six bytes cover multiple empty WebSocket control frames.
    let before = proxy.downstream_bytes.load(Ordering::Acquire);
    let until = Instant::now() + Duration::from_secs(20);
    while proxy.downstream_bytes.load(Ordering::Acquire) < before + 6 {
        assert!(
            Instant::now() < until,
            "the relay did not forward healthy traffic"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(link.shared.up.load(Ordering::Acquire));
    assert!(
        received
            .try_iter()
            .all(|event| !matches!(event, RelayLink::Connecting { .. }))
    );
    proxy.frozen.store(true, Ordering::Release);
    wait_link(&received, |event| {
        matches!(event,
        RelayLink::Connecting { error: Some(error), .. }
        if error == "the relay stopped answering; reconnecting")
    });
    assert!(!link.shared.up.load(Ordering::Acquire));
    proxy.frozen.store(false, Ordering::Release);
    wait_link(&received, |event| matches!(event, RelayLink::Up));
    assert!(link.shared.up.load(Ordering::Acquire));
    link.stop_and_wait();
}
