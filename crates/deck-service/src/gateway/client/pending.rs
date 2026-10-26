//! One-shot results of gateway requests: [`Pending`] is polled by the UI (`try_take`) or awaited
//! by a native worker (`wait`); [`Resolver`] is the client threads' side.

use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, Weak};
use std::time::{Duration, Instant};

use super::Wake;

enum State<T> {
    Waiting,
    Ready(T),
    Taken,
}

struct Cell<T> {
    state: Mutex<State<T>>,
    cv: Condvar,
}

impl<T> Cell<T> {
    fn lock(&self) -> MutexGuard<'_, State<T>> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// A result that arrives later. It resolves exactly once; a request whose link drops before
/// its reply resolves to `Err(GatewayError::Disconnected)`, never to a made-up success.
pub struct Pending<T> {
    cell: Arc<Cell<T>>,
}

impl<T> std::fmt::Debug for Pending<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pending")
            .field("ready", &self.is_ready())
            .finish()
    }
}

impl<T> Pending<T> {
    /// A pending result and the resolver the client threads complete it with.
    pub(crate) fn new(wake: Wake) -> (Pending<T>, Resolver<T>) {
        let cell = Arc::new(Cell {
            state: Mutex::new(State::Waiting),
            cv: Condvar::new(),
        });
        let resolver = Resolver {
            cell: Arc::downgrade(&cell),
            wake,
        };
        (Pending { cell }, resolver)
    }

    /// The result, once (later calls return `None`). Never blocks.
    pub fn try_take(&self) -> Option<T> {
        let mut st = self.cell.lock();
        match std::mem::replace(&mut *st, State::Taken) {
            State::Ready(v) => Some(v),
            other => {
                *st = other;
                None
            }
        }
    }

    /// The result arrived and was not taken yet.
    pub fn is_ready(&self) -> bool {
        matches!(*self.cell.lock(), State::Ready(_))
    }

    /// Blocks up to `timeout` for the result: for native worker threads only, never the UI
    /// thread. `None` when it did not arrive in time (or was already taken).
    pub fn wait(self, timeout: Duration) -> Option<T> {
        let deadline = Instant::now() + timeout;
        let mut st = self.cell.lock();
        loop {
            match std::mem::replace(&mut *st, State::Taken) {
                State::Ready(v) => return Some(v),
                State::Taken => return None,
                State::Waiting => *st = State::Waiting,
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return None;
            }
            st = self
                .cell
                .cv
                .wait_timeout(st, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }
}

/// Completes a [`Pending`]. Holds it weakly: when every `Pending` is gone nobody waits, and the
/// client drops its bookkeeping for it ([`Resolver::alive`]).
pub(crate) struct Resolver<T> {
    cell: Weak<Cell<T>>,
    wake: Wake,
}

impl<T> Resolver<T> {
    /// Someone still holds the `Pending`.
    pub(crate) fn alive(&self) -> bool {
        self.cell.strong_count() > 0
    }

    pub(crate) fn resolve(self, value: T) {
        let Some(cell) = self.cell.upgrade() else {
            return;
        };
        {
            let mut st = cell.lock();
            if matches!(*st, State::Waiting) {
                *st = State::Ready(value);
            }
        }
        cell.cv.notify_all();
        (self.wake)();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_once_and_wait_times_out_without_a_value() {
        let wake: Wake = Arc::new(|| {});
        let (p, r) = Pending::<u32>::new(wake.clone());
        assert!(!p.is_ready());
        assert_eq!(p.try_take(), None);
        r.resolve(7);
        assert!(p.is_ready());
        assert_eq!(p.try_take(), Some(7));
        assert_eq!(p.try_take(), None);

        let (p, r) = Pending::<u32>::new(wake.clone());
        assert!(r.alive());
        let waiter = std::thread::spawn(move || p.wait(Duration::from_secs(10)));
        std::thread::sleep(Duration::from_millis(20));
        r.resolve(9);
        assert_eq!(waiter.join().expect("join"), Some(9));

        let (p, r) = Pending::<u32>::new(wake);
        assert_eq!(p.wait(Duration::from_millis(10)), None);
        assert!(!r.alive(), "a dropped Pending is noticed");
    }
}
