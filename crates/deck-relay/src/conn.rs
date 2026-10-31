//! Per-connection outbound queue and inbound rate limits.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use axum::body::Bytes;
use deck_remote::relay::close;
use tokio::sync::Notify;

use crate::config::RateLimit;

/// The bounded outbound queue of one WebSocket. Producers never wait: a full queue closes the
/// connection with `SLOW_CONSUMER` instead, so no connection ever blocks on another's socket.
pub(crate) struct Conn {
    pub id: u64,
    state: Mutex<Outbox>,
    wake: Notify,
}

struct Outbox {
    /// Each message with the charge of the sender it is queued for (released when it leaves).
    queue: VecDeque<(Bytes, Option<Charged>)>,
    bytes: usize,
    max_messages: usize,
    max_bytes: usize,
    close: Option<u16>,
}

/// What one sender (a device token) has queued on connections (messages, bytes), so one sender's
/// backlog is bounded on its own, not only together with everyone else's.
#[derive(Default, Debug)]
pub(crate) struct Charge {
    messages: AtomicUsize,
    bytes: AtomicUsize,
}

impl Charge {
    pub fn queued(&self) -> (usize, usize) {
        (
            self.messages.load(Ordering::Relaxed),
            self.bytes.load(Ordering::Relaxed),
        )
    }
}

/// One queued message's share of a [`Charge`], released on drop (written, cleared, or the
/// connection dropped).
struct Charged {
    charge: Arc<Charge>,
    len: usize,
}

impl Drop for Charged {
    fn drop(&mut self) {
        self.charge.messages.fetch_sub(1, Ordering::Relaxed);
        self.charge.bytes.fetch_sub(self.len, Ordering::Relaxed);
    }
}

/// The queue overflowed; the connection is now closing with `SLOW_CONSUMER`.
#[derive(Debug)]
pub(crate) struct Overflow;

/// [`Conn::push_within`] refused a message (the connection stays open).
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Busy {
    /// The queue as a whole is above its share for such messages.
    Queue,
    /// The sender's own queued messages are above its share.
    Sender,
}

/// `share` (numerator, denominator) of `n`, rounded down.
fn part(n: usize, (num, den): (usize, usize)) -> usize {
    n / den * num + n % den * num / den
}

pub(crate) enum Next {
    Data(Bytes),
    Close(u16),
}

impl Conn {
    pub fn new(id: u64, max_messages: usize, max_bytes: usize) -> Conn {
        Conn {
            id,
            state: Mutex::new(Outbox {
                queue: VecDeque::new(),
                bytes: 0,
                max_messages,
                max_bytes,
                close: None,
            }),
            wake: Notify::new(),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Outbox> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Queues `msg`. A closing connection drops it silently.
    pub fn push(&self, msg: Bytes) -> Result<(), Overflow> {
        let mut s = self.lock();
        if s.close.is_some() {
            return Ok(());
        }
        if s.queue.len() >= s.max_messages || s.bytes + msg.len() > s.max_bytes {
            s.queue.clear();
            s.bytes = 0;
            s.close = Some(close::SLOW_CONSUMER);
            drop(s);
            self.wake.notify_one();
            return Err(Overflow);
        }
        s.bytes += msg.len();
        s.queue.push_back((msg, None));
        drop(s);
        self.wake.notify_one();
        Ok(())
    }

    /// Queues `msg` for the sender charged with `charge`, only while the whole queue stays within
    /// `queue_share` and the sender's queued messages within `sender_share` of the bounds (numerator,
    /// denominator), keeping the rest for other messages. Refusing does not close the connection;
    /// a closing connection drops `msg` silently.
    pub fn push_within(
        &self,
        msg: Bytes,
        queue_share: (usize, usize),
        charge: &Arc<Charge>,
        sender_share: (usize, usize),
    ) -> Result<(), Busy> {
        let mut s = self.lock();
        if s.close.is_some() {
            return Ok(());
        }
        let len = msg.len();
        let (messages, bytes) = charge.queued();
        if messages >= part(s.max_messages, sender_share)
            || bytes + len > part(s.max_bytes, sender_share)
        {
            return Err(Busy::Sender);
        }
        if s.queue.len() >= part(s.max_messages, queue_share)
            || s.bytes + len > part(s.max_bytes, queue_share)
        {
            return Err(Busy::Queue);
        }
        charge.messages.fetch_add(1, Ordering::Relaxed);
        charge.bytes.fetch_add(len, Ordering::Relaxed);
        s.bytes += len;
        s.queue.push_back((
            msg,
            Some(Charged {
                charge: charge.clone(),
                len,
            }),
        ));
        drop(s);
        self.wake.notify_one();
        Ok(())
    }

    /// Requests a close with `code`; the first request wins. Queued messages are still flushed
    /// (within the close timeout), except after `SLOW_CONSUMER`.
    pub fn close(&self, code: u16) {
        let mut s = self.lock();
        if s.close.is_some() {
            return;
        }
        s.close = Some(code);
        if code == close::SLOW_CONSUMER {
            s.queue.clear();
            s.bytes = 0;
        }
        drop(s);
        self.wake.notify_one();
    }

    pub fn close_code(&self) -> Option<u16> {
        self.lock().close
    }

    /// The next message to write: queued data first, then the close code.
    pub async fn next(&self) -> Next {
        loop {
            {
                let mut s = self.lock();
                if let Some((msg, charged)) = s.queue.pop_front() {
                    s.bytes -= msg.len();
                    // Handed to the writer: the sender's charge is released.
                    drop(charged);
                    return Next::Data(msg);
                }
                if let Some(code) = s.close {
                    return Next::Close(code);
                }
            }
            self.wake.notified().await;
        }
    }

    /// Resolves once a close was requested.
    pub async fn closing(&self) -> u16 {
        loop {
            if let Some(code) = self.close_code() {
                return code;
            }
            self.wake.notified().await;
        }
    }
}

/// Inbound rate limit of one connection.
pub(crate) struct Buckets {
    bytes: Bucket,
    messages: Bucket,
}

struct Bucket {
    rate: f64,
    burst: f64,
    tokens: f64,
}

impl Bucket {
    fn new(rate: u64, burst: u64) -> Bucket {
        Bucket {
            rate: rate as f64,
            burst: burst as f64,
            tokens: burst as f64,
        }
    }

    fn refill(&mut self, secs: f64) {
        self.tokens = (self.tokens + secs * self.rate).min(self.burst);
    }
}

impl Buckets {
    pub fn new(limit: RateLimit) -> Buckets {
        Buckets {
            bytes: Bucket::new(limit.bytes_per_sec, limit.bytes_burst),
            messages: Bucket::new(limit.messages_per_sec, limit.messages_burst),
        }
    }

    /// Charges one inbound message of `len` bytes; `false` if either bucket is empty.
    pub fn take(&mut self, len: usize, elapsed_since_last: f64) -> bool {
        self.bytes.refill(elapsed_since_last);
        self.messages.refill(elapsed_since_last);
        let len = len as f64;
        if self.bytes.tokens < len || self.messages.tokens < 1.0 {
            return false;
        }
        self.bytes.tokens -= len;
        self.messages.tokens -= 1.0;
        true
    }
}

/// [`Buckets`] with their own clock.
pub(crate) struct RateLimiter {
    buckets: Buckets,
    last: Instant,
}

impl RateLimiter {
    pub fn new(limit: RateLimit) -> RateLimiter {
        RateLimiter {
            buckets: Buckets::new(limit),
            last: Instant::now(),
        }
    }

    pub fn take(&mut self, len: usize) -> bool {
        let now = Instant::now();
        let secs = now.duration_since(self.last).as_secs_f64();
        self.last = now;
        self.buckets.take(len, secs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buckets_allow_bursts_and_refill() {
        let mut b = Buckets::new(RateLimit {
            bytes_per_sec: 100,
            bytes_burst: 300,
            messages_per_sec: 10,
            messages_burst: 2,
        });
        assert!(b.take(100, 0.0));
        assert!(b.take(100, 0.0));
        assert!(!b.take(1, 0.0), "message burst spent");
        assert!(b.take(100, 0.1));
        assert!(!b.take(200, 1.0), "bytes: 10 left + 100 refilled < 200");
        assert!(b.take(150, 0.5));
    }

    #[test]
    fn queue_overflow_closes_slow_consumer_and_drops_the_queue() {
        let c = Conn::new(1, 2, 10);
        c.push(Bytes::from_static(b"1234")).unwrap();
        c.push(Bytes::from_static(b"1234")).unwrap();
        assert!(c.push(Bytes::from_static(b"1")).is_err());
        assert_eq!(c.close_code(), Some(close::SLOW_CONSUMER));
        // Closing connections drop further messages silently.
        c.push(Bytes::from_static(b"1")).unwrap();
        let c = Conn::new(2, 8, 10);
        c.push(Bytes::from_static(b"123456")).unwrap();
        assert!(c.push(Bytes::from_static(b"12345")).is_err(), "byte bound");
    }

    #[test]
    fn push_within_keeps_headroom_without_closing() {
        let all = (1, 1);
        let c = Conn::new(1, 8, 100);
        let a = Arc::new(Charge::default());
        for _ in 0..6 {
            c.push_within(Bytes::from_static(b"1"), (3, 4), &a, all)
                .unwrap();
        }
        assert_eq!(
            c.push_within(Bytes::from_static(b"1"), (3, 4), &a, all),
            Err(Busy::Queue)
        );
        assert_eq!(c.close_code(), None, "refusing does not close");
        // The headroom is left for ordinary pushes.
        c.push(Bytes::from_static(b"1")).unwrap();
        c.push(Bytes::from_static(b"1")).unwrap();
        assert_eq!(c.close_code(), None);
        let c = Conn::new(2, 100, 100);
        c.push_within(Bytes::from(vec![0; 70]), (3, 4), &a, all)
            .unwrap();
        assert_eq!(
            c.push_within(Bytes::from(vec![0; 6]), (3, 4), &a, all),
            Err(Busy::Queue),
            "byte bound"
        );
        c.push_within(Bytes::from(vec![0; 5]), (3, 4), &a, all)
            .unwrap();
    }

    #[tokio::test]
    async fn each_sender_is_bounded_by_its_own_charge_until_its_messages_are_written() {
        let c = Conn::new(1, 8, 1000);
        let (flood, calm) = (Arc::new(Charge::default()), Arc::new(Charge::default()));
        for _ in 0..2 {
            c.push_within(Bytes::from_static(b"f"), (3, 4), &flood, (1, 4))
                .unwrap();
        }
        assert_eq!(
            c.push_within(Bytes::from_static(b"f"), (3, 4), &flood, (1, 4)),
            Err(Busy::Sender)
        );
        // Another sender still has its own share.
        c.push_within(Bytes::from_static(b"c"), (3, 4), &calm, (1, 4))
            .unwrap();
        assert_eq!(flood.queued(), (2, 2));
        // Written messages release their sender's charge.
        assert!(matches!(c.next().await, Next::Data(b) if b == "f"));
        assert_eq!(flood.queued(), (1, 1));
        c.push_within(Bytes::from_static(b"f"), (3, 4), &flood, (1, 4))
            .unwrap();
        // So do messages dropped with the queue (a slow consumer).
        c.close(close::SLOW_CONSUMER);
        assert_eq!(flood.queued(), (0, 0));
        assert_eq!(calm.queued(), (0, 0));
        // Byte share: 1000 / 4.
        let c = Conn::new(2, 100, 1000);
        let big = Arc::new(Charge::default());
        c.push_within(Bytes::from(vec![0; 200]), (3, 4), &big, (1, 4))
            .unwrap();
        assert_eq!(
            c.push_within(Bytes::from(vec![0; 51]), (3, 4), &big, (1, 4)),
            Err(Busy::Sender)
        );
        drop(c);
        assert_eq!(
            big.queued(),
            (0, 0),
            "a dropped connection releases charges"
        );
    }
}
