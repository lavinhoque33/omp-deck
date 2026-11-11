//! The bounded, purgeable delivery queue behind every push sender thread (Web Push and FCM): the
//! oldest push is dropped on overflow (overall and per device), a purge makes the queued pushes of
//! a device vanish and an attempt in flight for it final, and retries are capped.

use std::collections::{HashMap, VecDeque};
use std::io;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use ureq::http::HeaderValue;

use super::lock;

/// Pushes waiting at most (the oldest is dropped beyond).
pub const QUEUE: usize = 256;
/// Pushes waiting for one device at most (that device's oldest is dropped beyond), so a flood
/// for one device cannot evict the others' pushes.
pub const PER_DEVICE: usize = 16;
/// Tries per push (the first one included) for 429 and 5xx answers and transport failures.
pub const MAX_ATTEMPTS: u8 = 3;
/// The longest `Retry-After` honoured.
pub const MAX_RETRY_AFTER: Duration = Duration::from_secs(600);
/// A 429 without a usable `Retry-After` waits this long.
pub const DEFAULT_RETRY_AFTER: Duration = Duration::from_secs(60);

/// A queued push: which device it is for.
pub trait Routed {
    fn device(&self) -> &str;
}

pub struct Queued<J> {
    pub job: J,
    pub attempts: u8,
    pub due: Instant,
    /// The device's invalidation generation when the push was submitted.
    pub generation: u64,
}

pub struct State<J> {
    pub queue: VecDeque<Queued<J>>,
    /// Per device: how often it was purged. A push older than its device's generation is stale,
    /// including one whose attempt is in flight (it is outside `queue`).
    generations: HashMap<String, u64>,
    closed: bool,
}

impl<J> Default for State<J> {
    fn default() -> Self {
        State {
            queue: VecDeque::new(),
            generations: HashMap::new(),
            closed: false,
        }
    }
}

impl<J: Routed> State<J> {
    pub fn generation(&self, device: &str) -> u64 {
        self.generations.get(device).copied().unwrap_or(0)
    }

    pub fn is_stale(&self, item: &Queued<J>) -> bool {
        item.generation != self.generation(item.job.device())
    }

    pub fn purge(&mut self, device: &str) {
        *self.generations.entry(device.to_owned()).or_insert(0) += 1;
        purge(&mut self.queue, device);
    }
}

struct Shared<J> {
    state: Mutex<State<J>>,
    ready: Condvar,
}

/// What one delivery attempt came to.
pub enum Outcome {
    Sent,
    /// The push service reports the destination gone: [`Worker::gone`] is called.
    Gone,
    Retry(Duration),
    Failed,
}

/// The delivery half of a sender thread.
pub trait Worker<J>: Send + 'static {
    /// One try; `attempt` counts the earlier ones.
    fn deliver(&mut self, job: &J, attempt: u8) -> Outcome;
    /// The destination of `job` is gone.
    fn gone(&mut self, job: J);
}

/// The handle; dropping it stops the thread after its current request.
pub struct Handle<J> {
    shared: Arc<Shared<J>>,
}

impl<J: Routed + Send + 'static> Handle<J> {
    /// Runs `worker` on a thread called `name`.
    pub fn spawn(name: &str, worker: impl Worker<J>) -> io::Result<Handle<J>> {
        let shared = Arc::new(Shared {
            state: Mutex::new(State::default()),
            ready: Condvar::new(),
        });
        let theirs = shared.clone();
        std::thread::Builder::new()
            .name(name.to_owned())
            .spawn(move || run(worker, &theirs))?;
        Ok(Handle { shared })
    }

    pub fn send(&self, job: J) {
        let mut st = lock(&self.shared.state);
        let generation = st.generation(job.device());
        enqueue(
            &mut st.queue,
            Queued {
                job,
                attempts: 0,
                due: Instant::now(),
                generation,
            },
        );
        drop(st);
        self.shared.ready.notify_one();
    }

    /// Drops everything still queued for `device` (revoked, unsubscribed or resubscribed) and
    /// makes any attempt in flight for it final: it is not retried.
    pub fn purge(&self, device: &str) {
        lock(&self.shared.state).purge(device);
    }
}

impl<J> Drop for Handle<J> {
    fn drop(&mut self) {
        lock(&self.shared.state).closed = true;
        self.shared.ready.notify_one();
    }
}

fn run<J: Routed>(mut worker: impl Worker<J>, shared: &Shared<J>) {
    while let Some(mut item) = next(shared) {
        if lock(&shared.state).is_stale(&item) {
            continue;
        }
        match worker.deliver(&item.job, item.attempts) {
            Outcome::Sent | Outcome::Failed => {}
            Outcome::Gone => {
                // A purge since the push was submitted (the device resubscribed or was
                // unsubscribed) makes the report about a destination nobody uses any more.
                if !lock(&shared.state).is_stale(&item) {
                    worker.gone(item.job);
                }
            }
            Outcome::Retry(wait) => {
                item.attempts += 1;
                if item.attempts >= MAX_ATTEMPTS {
                    log::warn!(
                        "push to {}: dropped after {MAX_ATTEMPTS} attempts",
                        item.job.device()
                    );
                    continue;
                }
                item.due = Instant::now() + wait;
                let mut st = lock(&shared.state);
                if st.is_stale(&item) {
                    continue;
                }
                enqueue(&mut st.queue, item);
            }
        }
    }
}

pub fn purge<J: Routed>(queue: &mut VecDeque<Queued<J>>, device: &str) {
    queue.retain(|q| q.job.device() != device);
}

pub fn enqueue<J: Routed>(queue: &mut VecDeque<Queued<J>>, item: Queued<J>) {
    if queue
        .iter()
        .filter(|q| q.job.device() == item.job.device())
        .count()
        >= PER_DEVICE
        && let Some(i) = queue
            .iter()
            .position(|q| q.job.device() == item.job.device())
    {
        queue.remove(i);
        log::warn!(
            "push queue full for {}: dropped its oldest push",
            item.job.device()
        );
    }
    if queue.len() >= QUEUE
        && let Some(old) = queue.pop_front()
    {
        log::warn!(
            "push queue full: dropped the oldest push (to {})",
            old.job.device()
        );
    }
    queue.push_back(item);
}

/// A `Retry-After` in seconds, capped at [`MAX_RETRY_AFTER`].
pub fn retry_after(value: Option<&HeaderValue>) -> Duration {
    value
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map_or(DEFAULT_RETRY_AFTER, Duration::from_secs)
        .min(MAX_RETRY_AFTER)
}

/// The next due push; `None` once the sender is dropped.
fn next<J: Routed>(shared: &Shared<J>) -> Option<Queued<J>> {
    let mut st = lock(&shared.state);
    loop {
        if st.closed {
            return None;
        }
        let now = Instant::now();
        let first = st
            .queue
            .iter()
            .enumerate()
            .min_by_key(|(_, q)| q.due)
            .map(|(i, q)| (i, q.due));
        st = match first {
            Some((i, due)) if due <= now => return st.queue.remove(i),
            Some((_, due)) => {
                shared
                    .ready
                    .wait_timeout(st, due - now)
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .0
            }
            None => shared
                .ready
                .wait(st)
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Item(String);

    impl Routed for Item {
        fn device(&self) -> &str {
            &self.0
        }
    }

    fn queued(device: &str) -> Queued<Item> {
        Queued {
            job: Item(device.to_owned()),
            attempts: 0,
            due: Instant::now(),
            generation: 0,
        }
    }

    #[test]
    fn a_full_queue_drops_its_oldest_push() {
        let mut q = VecDeque::new();
        for i in 0..QUEUE + 2 {
            enqueue(&mut q, queued(&i.to_string()));
        }
        assert_eq!(q.len(), QUEUE);
        assert_eq!(q.front().unwrap().job.0, "2");
        assert_eq!(q.back().unwrap().job.0, (QUEUE + 1).to_string());
    }

    #[test]
    fn one_devices_flood_keeps_to_its_own_cap() {
        let mut q = VecDeque::new();
        enqueue(&mut q, queued("other"));
        for _ in 0..PER_DEVICE * 3 {
            enqueue(&mut q, queued("flood"));
        }
        assert_eq!(q.iter().filter(|x| x.job.0 == "flood").count(), PER_DEVICE);
        assert!(q.iter().any(|x| x.job.0 == "other"));
    }

    #[test]
    fn purging_a_device_drops_only_its_pushes() {
        let mut q = VecDeque::new();
        for d in ["a", "b", "a"] {
            enqueue(&mut q, queued(d));
        }
        purge(&mut q, "a");
        assert_eq!(q.len(), 1);
        assert_eq!(q.front().unwrap().job.0, "b");
    }

    #[test]
    fn a_purge_makes_an_in_flight_push_stale() {
        let mut st = State::default();
        let mut item = queued("a");
        item.generation = st.generation("a");
        assert!(!st.is_stale(&item));
        // The item is "in flight": outside the queue when the purge happens.
        st.purge("a");
        assert!(st.is_stale(&item));
        assert!(!st.is_stale(&queued("b")));
        let mut fresh = queued("a");
        fresh.generation = st.generation("a");
        assert!(!st.is_stale(&fresh));
    }

    #[test]
    fn retry_after_is_honoured_up_to_ten_minutes() {
        let v = |s: &'static str| HeaderValue::from_static(s);
        assert_eq!(retry_after(Some(&v("30"))), Duration::from_secs(30));
        assert_eq!(retry_after(Some(&v("86400"))), MAX_RETRY_AFTER);
        assert_eq!(retry_after(Some(&v("soon"))), DEFAULT_RETRY_AFTER);
        assert_eq!(retry_after(None), DEFAULT_RETRY_AFTER);
    }
}
