//! Per-connection outbound queue and inbound rate limits.

use std::collections::VecDeque;
use std::sync::Mutex;
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
    queue: VecDeque<Bytes>,
    bytes: usize,
    max_messages: usize,
    max_bytes: usize,
    close: Option<u16>,
}

/// The queue overflowed; the connection is now closing with `SLOW_CONSUMER`.
#[derive(Debug)]
pub(crate) struct Overflow;

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
        s.queue.push_back(msg);
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
                if let Some(msg) = s.queue.pop_front() {
                    s.bytes -= msg.len();
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
}
