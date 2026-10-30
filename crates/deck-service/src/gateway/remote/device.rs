//! A paired device's application session (`docs/notes/mobile-relay.md`, "R4 contract"), the
//! relay link's half: [`DeviceOut`], the queue the engine fills (replies, a bounded event queue,
//! latest-only feed slots, view changes), and [`Runtime`], which the link thread runs per device
//! session. The runtime decodes device records and answers the malformed ones itself, batches
//! gateway records into messages within the device's acknowledgement window, pulls terminal
//! output only while the window has room, and detects a device that stopped acknowledging.

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use deck_remote::api::{
    self, ApiError, Event, Feed, InputRefusal, MAX_APP_MESSAGE, MAX_REPLY, MAX_SNAPSHOT,
    MAX_SNAPSHOT_CHUNK, Op, Record, Reply, ReplyResult, Request, RequestError, TermEnd, ViewMode,
};
use deck_remote::relay::close;
use deck_term::{OutputEvent, OutputSubscription};

use crate::controller::Rejected;
use crate::gateway::protocol::{InputRefused, ViewEnd};
use crate::gateway::server::{SideItem, ViewSide};

/// Overrides [`api::STALL_SECS`] (milliseconds; tests).
pub const STALL_ENV: &str = "OMP_DECK_DEVICE_STALL_MS";
/// Bytes of one `T_OUTPUT` record at most.
const OUTPUT_CHUNK: usize = 256 * 1024;
/// Cell size handed to the terminal with a device's resize (devices report cells only).
const CELL: (u16, u16) = (8, 16);
/// The largest terminal a device may size a session to: a phone screen is far smaller, and a
/// huge grid would make the terminal allocate gigabytes.
pub const MAX_COLS: u16 = 500;
pub const MAX_ROWS: u16 = 200;

/// A device's requested terminal size, within `1..=MAX_COLS` × `1..=MAX_ROWS`.
pub fn device_size(cols: u16, rows: u16) -> (u16, u16) {
    (cols.clamp(1, MAX_COLS), rows.clamp(1, MAX_ROWS))
}
/// Terminal traffic (input, resize) of one device handed to the engine and not processed yet,
/// in budget units: its bytes plus [`RECORD_COST`] per engine message. Input beyond it is
/// refused on the link (`input_refused` `rejected`), so a flood never queues on the engine.
const TERMINAL_BACKLOG: usize = 256 * 1024;
/// Budget units one terminal message costs beyond its bytes (bounds the message count).
const RECORD_COST: usize = 64;
/// Replies the link answers itself (malformed, unsupported, too many in flight) and has not
/// sent yet. A device that piles up more while its window is full is closed `APP_PROTOCOL`.
const LOCAL_REPLIES: usize = 256;
const LOCAL_BYTES: usize = 256 * 1024;
/// Views the link holds for one device at most, open or ended with their `T_END` unsent:
/// detaching frees the engine's view slot at once, so attaches beyond this are refused
/// (`too_many_requests`) until the ends went out.
pub const RETAINED_VIEWS: usize = 2 * api::MAX_VIEWS as usize;

pub type Wake = Arc<dyn Fn() + Send + Sync>;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn json(value: &impl serde::Serialize) -> Option<Vec<u8>> {
    match serde_json::to_vec(value) {
        Ok(b) => Some(b),
        Err(e) => {
            log::error!("device protocol: encoding failed: {e}");
            None
        }
    }
}

/// A `REPLY` body: `result`, or `too_large` when it does not fit one record.
pub fn reply_body(id: u64, result: ReplyResult) -> Option<Vec<u8>> {
    let body = json(&Reply { id, result })?;
    if body.len() <= MAX_REPLY {
        return Some(body);
    }
    json(&Reply {
        id,
        result: ReplyResult::Err(ApiError::TooLarge {}),
    })
}

/// How long a device may leave messages unacknowledged.
fn stall_limit() -> Duration {
    std::env::var(STALL_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&ms| ms > 0)
        .map_or(Duration::from_secs(api::STALL_SECS), Duration::from_millis)
}

// ---- engine → link ------------------------------------------------------------------------------

/// A view the engine attached for the device.
pub struct NewView {
    pub id: u32,
    /// The engine's view id.
    pub engine: u64,
    pub side: Arc<ViewSide>,
    /// Observe views never type; only control views resize.
    pub mode: ViewMode,
}

#[derive(Default)]
struct OutQ {
    /// `WELCOME` and the engine's `REPLY`s, in order.
    control: VecDeque<Record>,
    /// `EVENT` bodies.
    events: VecDeque<Vec<u8>>,
    event_bytes: usize,
    /// Latest `EVENT` (`feed`) body per feed, in the order the feeds first got a value.
    feeds: Vec<(Feed, Vec<u8>)>,
    views: Vec<NewView>,
    /// Views the engine ended (detach, lost permission).
    ends: Vec<(u32, TermEnd)>,
    /// Views attached and not yet let go of by the link (open, or ended with their `T_END`
    /// not sent yet).
    retained: usize,
    closed: bool,
}

/// What the engine queues for one device session. Shared with the link thread, which takes
/// from it as the device's window allows.
pub struct DeviceOut {
    q: Mutex<OutQ>,
    wake_link: Wake,
    wake_engine: Wake,
}

impl DeviceOut {
    /// `wake_link` makes the link thread pull; `wake_engine` runs when the link took a
    /// transcript value (its next chunk can be queued).
    pub fn new(wake_link: Wake, wake_engine: Wake) -> Arc<DeviceOut> {
        Arc::new(DeviceOut {
            q: Mutex::new(OutQ::default()),
            wake_link,
            wake_engine,
        })
    }

    pub fn link_waker(&self) -> Wake {
        self.wake_link.clone()
    }

    fn with(&self, f: impl FnOnce(&mut OutQ)) {
        {
            let mut q = lock(&self.q);
            if q.closed {
                return;
            }
            f(&mut q);
        }
        (self.wake_link)();
    }

    pub fn welcome(&self, welcome: &api::Welcome) {
        if let Some(body) = json(welcome) {
            self.with(|q| q.control.push_back(Record::Welcome(body)));
        }
    }

    pub fn reply(&self, id: u64, result: ReplyResult) {
        if let Some(body) = reply_body(id, result) {
            self.with(|q| q.control.push_back(Record::Reply(body)));
        }
    }

    /// Queues an event; a full queue is cleared for one [`Event::Resync`].
    pub fn event(&self, event: &Event) {
        let Some(body) = json(event) else {
            return;
        };
        if body.len() > MAX_REPLY {
            log::warn!("device protocol: dropping an oversized event");
            return;
        }
        self.with(|q| {
            if q.events.len() >= api::EVENT_QUEUE || q.event_bytes + body.len() > api::EVENT_BYTES {
                q.events.clear();
                q.event_bytes = 0;
                if let Some(resync) = json(&Event::Resync {}) {
                    q.event_bytes += resync.len();
                    q.events.push_back(resync);
                }
            }
            q.event_bytes += body.len();
            q.events.push_back(body);
        });
    }

    /// Replaces `feed`'s pending value with `body` (an encoded [`Event::Feed`]).
    pub fn feed(&self, feed: &Feed, body: Vec<u8>) {
        self.with(|q| match q.feeds.iter_mut().find(|(f, _)| f == feed) {
            Some((_, slot)) => *slot = body,
            None => q.feeds.push((feed.clone(), body)),
        });
    }

    /// `feed` has a value the link did not take yet.
    pub fn feed_pending(&self, feed: &Feed) -> bool {
        lock(&self.q).feeds.iter().any(|(f, _)| f == feed)
    }

    pub fn drop_feed(&self, feed: &Feed) {
        lock(&self.q).feeds.retain(|(f, _)| f != feed);
    }

    pub fn attach(&self, view: NewView) {
        self.with(|q| {
            q.retained += 1;
            q.views.push(view);
        });
    }

    /// Another view may be attached: the link holds fewer than [`RETAINED_VIEWS`] (detached
    /// views count until their `T_END` went out).
    pub fn view_room(&self) -> bool {
        lock(&self.q).retained < RETAINED_VIEWS
    }

    /// The link let go of `n` views.
    fn released(&self, n: usize) {
        let mut q = lock(&self.q);
        q.retained = q.retained.saturating_sub(n);
    }

    pub fn end_view(&self, view: u32, end: TermEnd) {
        self.with(|q| q.ends.push((view, end)));
    }

    /// The session is over: nothing more is queued.
    pub fn close(&self) {
        let mut q = lock(&self.q);
        *q = OutQ::default();
        q.closed = true;
    }
}

// ---- link side ----------------------------------------------------------------------------------

/// What a device asked of the engine.
#[derive(Debug)]
pub enum Inbound {
    Request(Box<Request>),
    /// Terminal input for the engine's view `view` (consecutive input records of one message
    /// coalesced).
    Input {
        view: u64,
        bytes: Vec<u8>,
        ctrl_c: bool,
        credit: Credit,
    },
    Resize {
        view: u64,
        cols: u16,
        rows: u16,
        cell_width: u16,
        cell_height: u16,
        credit: Credit,
    },
    /// The engine's view `view` ended on the link side (output end, overflow, too large).
    ViewGone(u64),
}

/// A share of a device's [`TERMINAL_BACKLOG`], returned when the engine drops what holds it.
#[derive(Debug)]
pub struct Credit {
    pool: Arc<AtomicUsize>,
    units: usize,
}

impl Credit {
    /// `units` more, if the backlog has room.
    fn take(pool: &Arc<AtomicUsize>, units: usize) -> Option<Credit> {
        // The link thread is the only one adding; the engine only returns.
        if pool.load(Ordering::Acquire) + units > TERMINAL_BACKLOG {
            return None;
        }
        pool.fetch_add(units, Ordering::AcqRel);
        Some(Credit {
            pool: pool.clone(),
            units,
        })
    }

    fn grow(&mut self, units: usize) -> bool {
        match Credit::take(&self.pool, units) {
            Some(mut more) => {
                self.units += std::mem::take(&mut more.units);
                true
            }
            None => false,
        }
    }
}

impl Drop for Credit {
    fn drop(&mut self) {
        self.pool.fetch_sub(self.units, Ordering::AcqRel);
    }
}

/// Gateway egress over every device, paced below the relay's limits.
pub struct Pace {
    bytes: f64,
    frames: f64,
    at: Instant,
}

impl Pace {
    const BURST: f64 = 0.25;

    pub fn new(now: Instant) -> Pace {
        Pace {
            bytes: api::EGRESS_BYTES_PER_SEC as f64 * Self::BURST,
            frames: api::EGRESS_FRAMES_PER_SEC as f64 * Self::BURST,
            at: now,
        }
    }

    fn refill(&mut self, now: Instant) {
        let dt = now.saturating_duration_since(self.at).as_secs_f64();
        self.at = now;
        let (bmax, fmax) = (
            api::EGRESS_BYTES_PER_SEC as f64 * Self::BURST,
            api::EGRESS_FRAMES_PER_SEC as f64 * Self::BURST,
        );
        self.bytes = (self.bytes + dt * api::EGRESS_BYTES_PER_SEC as f64).min(bmax);
        self.frames = (self.frames + dt * api::EGRESS_FRAMES_PER_SEC as f64).min(fmax);
    }

    /// Whether another message may go out now.
    pub fn open(&mut self, now: Instant) -> bool {
        self.refill(now);
        self.bytes > 0.0 && self.frames > 0.0
    }

    /// Charges what was sent (the balance may go negative: the next message waits).
    pub fn spend(&mut self, bytes: usize, frames: usize) {
        self.bytes -= bytes as f64;
        self.frames -= frames as f64;
    }
}

struct LinkView {
    engine: u64,
    /// The view's terminal side and output; both dropped once the view ends.
    side: Option<Arc<ViewSide>>,
    mode: ViewMode,
    sub: Option<Arc<OutputSubscription>>,
    /// Records of this view not sent yet (snapshot chunks, output pieces), in order.
    pending: VecDeque<Record>,
    /// Its `T_END` is queued: nothing more is pulled.
    ended: bool,
}

impl LinkView {
    /// Queues `end` and lets go of the terminal: only the queued records stay.
    fn end(&mut self, id: u32, end: &TermEnd) {
        self.ended = true;
        self.side = None;
        self.sub = None;
        push_end(&mut self.pending, id, end);
    }
}

/// One device session on the link thread.
pub struct Runtime {
    out: Arc<DeviceOut>,
    /// Replies the link gives itself (malformed requests, too many in flight).
    local: VecDeque<Record>,
    local_bytes: usize,
    /// Gateway messages sent so far, and how many the device acknowledged.
    sent: u64,
    acked: u64,
    /// Unacknowledged messages (number, bytes), oldest first.
    unacked: VecDeque<(u64, usize)>,
    unacked_bytes: usize,
    /// Since when the oldest unacknowledged message waits (reset by every ack).
    waiting_since: Option<Instant>,
    stall: Duration,
    /// Requests handed to the engine and not answered yet.
    inflight: u32,
    /// [`TERMINAL_BACKLOG`] units held by terminal messages the engine has not dropped yet.
    backlog: Arc<AtomicUsize>,
    views: BTreeMap<u32, LinkView>,
    /// Round-robin start among the views.
    turn: u32,
}

impl Runtime {
    pub fn new(out: Arc<DeviceOut>) -> Runtime {
        Runtime {
            out,
            local: VecDeque::new(),
            local_bytes: 0,
            sent: 0,
            acked: 0,
            unacked: VecDeque::new(),
            unacked_bytes: 0,
            waiting_since: None,
            stall: stall_limit(),
            inflight: 0,
            backlog: Arc::new(AtomicUsize::new(0)),
            views: BTreeMap::new(),
            turn: 0,
        }
    }

    /// Queues a reply of the link's own; `Err` (close code) once a device piles up more than
    /// the link holds for it.
    fn answer(&mut self, id: u64, err: ApiError) -> Result<(), u16> {
        let Some(body) = reply_body(id, ReplyResult::Err(err)) else {
            return Ok(());
        };
        if self.local.len() >= LOCAL_REPLIES || self.local_bytes + body.len() > LOCAL_BYTES {
            return Err(close::APP_PROTOCOL);
        }
        self.local_bytes += body.len();
        self.local.push_back(Record::Reply(body));
        Ok(())
    }

    /// Handles one decrypted device message. `Err` is the close code: the message was not a
    /// whole sequence of device records, a request was unreadable, or the device piled up
    /// more local replies than the link holds.
    ///
    /// Terminal records are admitted here, never queued on the engine without bound: input to
    /// a view that may not type is refused, consecutive inputs of one view are coalesced into
    /// one engine message (resizes keep only the latest per view), and everything is charged
    /// to the device's [`TERMINAL_BACKLOG`] until the engine processed it.
    pub fn receive(&mut self, message: &[u8], now: Instant) -> Result<Vec<Inbound>, u16> {
        let records = api::decode(message, api::Origin::Device).map_err(|_| close::APP_PROTOCOL)?;
        self.sync_views();
        let mut inbound = Vec::new();
        // One `input_refused` per known view and message (the views are few: see
        // `RETAINED_VIEWS`), and one in all for ids this link does not hold, however many.
        let mut refused: Vec<(u32, InputRefusal)> = Vec::new();
        let mut refuse = |view: u32, reason: InputRefusal| {
            if !refused.iter().any(|(v, _)| *v == view) {
                refused.push((view, reason));
            }
        };
        let mut unknown: Option<u32> = None;
        // Where this message's resize of an engine view is (one per control view).
        let mut resized: Vec<(u64, usize)> = Vec::new();
        for record in records {
            match record {
                Record::Request(body) => match Request::parse(&body) {
                    Err(RequestError::Unreadable) => return Err(close::APP_PROTOCOL),
                    Err(RequestError::Unsupported { id }) => {
                        self.answer(id, ApiError::Unsupported {})?;
                    }
                    Err(RequestError::Bad { id, message }) => {
                        self.answer(id, ApiError::BadRequest { message })?;
                    }
                    Ok(req) => {
                        let bounds = match &req.op {
                            Op::Subscribe { feed } | Op::Unsubscribe { feed } => feed.check(),
                            _ => Ok(()),
                        };
                        if let Err(message) = req.check_seq().and(bounds) {
                            self.answer(req.id, ApiError::BadRequest { message })?;
                        } else if self.inflight >= api::MAX_INFLIGHT {
                            self.answer(req.id, ApiError::TooManyRequests {})?;
                        } else {
                            self.inflight += 1;
                            inbound.push(Inbound::Request(Box::new(req)));
                        }
                    }
                },
                Record::Ack { messages } => {
                    if messages > self.sent {
                        return Err(close::APP_PROTOCOL);
                    }
                    if messages > self.acked {
                        self.acked = messages;
                        while self.unacked.front().is_some_and(|(n, _)| *n <= messages) {
                            if let Some((_, bytes)) = self.unacked.pop_front() {
                                self.unacked_bytes -= bytes;
                            }
                        }
                        self.waiting_since = (!self.unacked.is_empty()).then_some(now);
                    }
                }
                Record::Input {
                    view,
                    ctrl_c,
                    bytes,
                } => {
                    if bytes.is_empty() && !ctrl_c {
                        continue;
                    }
                    let Some(v) = self.views.get(&view) else {
                        unknown.get_or_insert(view);
                        continue;
                    };
                    if v.ended {
                        refuse(view, InputRefusal::Detached);
                        continue;
                    }
                    if v.mode == ViewMode::Observe {
                        refuse(view, InputRefusal::NotAllowed);
                        continue;
                    }
                    let engine = v.engine;
                    // A plain input continues the previous plain input of the same view.
                    if !ctrl_c
                        && let Some(Inbound::Input {
                            view: last,
                            bytes: earlier,
                            ctrl_c: false,
                            credit,
                        }) = inbound.last_mut()
                        && *last == engine
                    {
                        if credit.grow(bytes.len()) {
                            earlier.extend_from_slice(&bytes);
                        } else {
                            refuse(view, InputRefusal::Rejected);
                        }
                        continue;
                    }
                    match Credit::take(&self.backlog, bytes.len() + RECORD_COST) {
                        Some(credit) => inbound.push(Inbound::Input {
                            view: engine,
                            bytes,
                            ctrl_c,
                            credit,
                        }),
                        None => refuse(view, InputRefusal::Rejected),
                    }
                }
                Record::Resize { view, cols, rows } => {
                    let (cols, rows) = device_size(cols, rows);
                    let Some(v) = self
                        .views
                        .get(&view)
                        .filter(|v| !v.ended && v.mode == ViewMode::Control)
                    else {
                        continue;
                    };
                    let engine = v.engine;
                    let earlier = resized
                        .iter()
                        .find(|(e, _)| *e == engine)
                        .and_then(|(_, at)| match inbound.get_mut(*at) {
                            Some(Inbound::Resize {
                                cols: c, rows: r, ..
                            }) => Some((c, r)),
                            _ => None,
                        });
                    if let Some((c, r)) = earlier {
                        (*c, *r) = (cols, rows);
                    } else if let Some(credit) = Credit::take(&self.backlog, RECORD_COST) {
                        resized.push((engine, inbound.len()));
                        inbound.push(Inbound::Resize {
                            view: engine,
                            cols,
                            rows,
                            cell_width: CELL.0,
                            cell_height: CELL.1,
                            credit,
                        });
                    }
                }
                // `decode` with `Origin::Device` yields device kinds only.
                _ => return Err(close::APP_PROTOCOL),
            }
        }
        for (view, reason) in refused {
            self.out.event(&Event::InputRefused { view, reason });
        }
        if let Some(view) = unknown {
            self.out.event(&Event::InputRefused {
                view,
                reason: InputRefusal::Detached,
            });
        }
        Ok(inbound)
    }

    /// Messages are outstanding and none was acknowledged for too long.
    pub fn stalled(&self, now: Instant) -> bool {
        self.waiting_since
            .is_some_and(|t| now.saturating_duration_since(t) >= self.stall)
    }

    fn window_open(&self) -> bool {
        self.unacked.len() < api::WINDOW_MESSAGES as usize
            && self.unacked_bytes < api::WINDOW_BYTES as usize
    }

    /// Takes the views the engine attached or ended (so input finds them at once).
    fn sync_views(&mut self) {
        let mut q = lock(&self.out.q);
        for v in q.views.drain(..) {
            self.views.insert(
                v.id,
                LinkView {
                    engine: v.engine,
                    side: Some(v.side),
                    mode: v.mode,
                    sub: None,
                    pending: VecDeque::new(),
                    ended: false,
                },
            );
        }
        for (id, end) in q.ends.drain(..) {
            // A view that already ended keeps its queued `T_END` (or already sent it).
            if let Some(v) = self.views.get_mut(&id)
                && !v.ended
            {
                v.pending.clear();
                v.end(id, &end);
            }
        }
    }

    /// Sends the records of views that ended (their `T_END`, little else) ahead of anything
    /// else, so ended views never linger behind replies; drops each one once sent.
    fn flush_ended(&mut self, batch: &mut Batch, gone: &mut Vec<u64>) {
        let mut released = 0;
        self.views.retain(|_, v| {
            if !v.ended {
                return true;
            }
            while let Some(r) = v.pending.front() {
                if !batch.fits(r) {
                    return true;
                }
                if let Some(r) = v.pending.pop_front() {
                    batch.push(r);
                }
            }
            gone.push(v.engine);
            released += 1;
            false
        });
        self.out.released(released);
    }

    /// Bytes the next message may hold: the window's remaining byte credit, or a whole
    /// message when nothing is outstanding (so a large record always gets through).
    fn credit(&self) -> usize {
        if self.unacked.is_empty() {
            MAX_APP_MESSAGE
        } else {
            (api::WINDOW_BYTES as usize)
                .saturating_sub(self.unacked_bytes)
                .min(MAX_APP_MESSAGE)
        }
    }

    /// The next application message, if the window allows one and anything is ready; the
    /// views that ended on this side are added to `gone`.
    pub fn next_message(&mut self, now: Instant, gone: &mut Vec<u64>) -> Option<Vec<u8>> {
        self.sync_views();
        if !self.window_open() {
            return None;
        }
        let mut batch = Batch::new(self.credit());
        // `WELCOME` is always the session's first record.
        {
            let mut q = lock(&self.out.q);
            if matches!(q.control.front(), Some(Record::Welcome(_)))
                && let Some(w) = q.control.pop_front()
            {
                batch.push(w);
            }
        }
        // Ended views' `T_END`s, then replies, events and feed values, then terminal data.
        self.flush_ended(&mut batch, gone);
        while let Some(r) = self.local.front() {
            if !batch.fits(r) {
                return self.finish(batch, now);
            }
            if let Some(r) = self.local.pop_front() {
                if let Record::Reply(b) = &r {
                    self.local_bytes -= b.len();
                }
                batch.push(r);
            }
        }
        let mut transcript_taken = false;
        let full = {
            let mut q = lock(&self.out.q);
            let mut full = false;
            while let Some(r) = q.control.front() {
                if !batch.fits(r) {
                    full = true;
                    break;
                }
                if let Some(r) = q.control.pop_front() {
                    if matches!(r, Record::Reply(_)) {
                        self.inflight = self.inflight.saturating_sub(1);
                    }
                    batch.push(r);
                }
            }
            while !full && let Some(body) = q.events.front() {
                let r = Record::Event(body.clone());
                if !batch.fits(&r) {
                    full = true;
                    break;
                }
                if let Some(b) = q.events.pop_front() {
                    q.event_bytes -= b.len();
                }
                batch.push(r);
            }
            while !full && !q.feeds.is_empty() {
                let r = Record::Event(q.feeds[0].1.clone());
                if !batch.fits(&r) {
                    full = true;
                    break;
                }
                let (feed, _) = q.feeds.remove(0);
                transcript_taken |= matches!(feed, Feed::Transcript { .. });
                batch.push(r);
            }
            full
        };
        if transcript_taken {
            (self.out.wake_engine)();
        }
        if !full {
            self.pull_views(&mut batch, gone);
        }
        self.finish(batch, now)
    }

    fn finish(&mut self, batch: Batch, now: Instant) -> Option<Vec<u8>> {
        if batch.records.is_empty() {
            return None;
        }
        match api::encode(&batch.records) {
            Ok(msg) => {
                self.sent += 1;
                self.unacked.push_back((self.sent, msg.len()));
                self.unacked_bytes += msg.len();
                if self.waiting_since.is_none() {
                    self.waiting_since = Some(now);
                }
                Some(msg)
            }
            Err(e) => {
                log::error!("device protocol: a message did not encode: {e}");
                None
            }
        }
    }

    /// Terminal data of every view, round robin, while the message has room.
    fn pull_views(&mut self, batch: &mut Batch, gone: &mut Vec<u64>) {
        let ids: Vec<u32> = self
            .views
            .range(self.turn..)
            .chain(self.views.range(..self.turn))
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            let Some(v) = self.views.get_mut(&id) else {
                continue;
            };
            let full = fill_view(id, v, batch);
            if v.ended && v.pending.is_empty() {
                gone.push(v.engine);
                self.views.remove(&id);
                self.out.released(1);
            }
            if full {
                self.turn = id.wrapping_add(1);
                return;
            }
        }
    }
}

/// Moves `v`'s records into `batch`, pulling more output while there is room; `true` = full.
fn fill_view(id: u32, v: &mut LinkView, batch: &mut Batch) -> bool {
    loop {
        while let Some(r) = v.pending.front() {
            if !batch.fits(r) {
                return true;
            }
            if let Some(r) = v.pending.pop_front() {
                batch.push(r);
            }
        }
        let Some(side) = v.side.as_ref().filter(|_| !v.ended) else {
            return false;
        };
        let (items, sub) = side.take();
        if v.sub.is_none() {
            v.sub = sub;
        }
        for item in items {
            match item {
                SideItem::Refused(r) => {
                    let reason = refusal(&r);
                    if let Some(body) = json(&Event::InputRefused { view: id, reason }) {
                        v.pending.push_back(Record::Event(body));
                    }
                }
                SideItem::Ended(end) => {
                    v.end(id, &term_end(end));
                    break;
                }
            }
        }
        if v.ended || !v.pending.is_empty() {
            continue;
        }
        match v.sub.as_ref().and_then(|s| s.try_recv()) {
            Some(ev) => output(id, v, ev),
            None => return false,
        }
    }
}

/// Queues one output event of view `id` as records.
fn output(id: u32, v: &mut LinkView, ev: OutputEvent) {
    match ev {
        OutputEvent::Resize { cols, rows, .. } => v.pending.push_back(Record::TermResize {
            view: id,
            cols,
            rows,
        }),
        OutputEvent::Snapshot(bytes) => {
            if bytes.len() > MAX_SNAPSHOT {
                v.end(id, &TermEnd::TooLarge {});
                return;
            }
            let mut chunks = bytes.chunks(MAX_SNAPSHOT_CHUNK).peekable();
            if chunks.peek().is_none() {
                v.pending.push_back(Record::Snapshot {
                    view: id,
                    last: true,
                    bytes: Vec::new(),
                });
            }
            while let Some(c) = chunks.next() {
                v.pending.push_back(Record::Snapshot {
                    view: id,
                    last: chunks.peek().is_none(),
                    bytes: c.to_vec(),
                });
            }
        }
        OutputEvent::Output(bytes) => {
            for c in bytes.chunks(OUTPUT_CHUNK) {
                v.pending.push_back(Record::Output {
                    view: id,
                    bytes: c.to_vec(),
                });
            }
        }
        OutputEvent::State(state) => {
            if let Some(body) = json(&Event::TermState {
                view: id,
                input_held: state.input_held,
            }) {
                v.pending.push_back(Record::Event(body));
            }
        }
        OutputEvent::Exited(code) => v.end(id, &TermEnd::Exited { code }),
        OutputEvent::Overflow => v.end(id, &TermEnd::Overflow {}),
        OutputEvent::Closed => v.end(id, &TermEnd::Closed {}),
    }
}

fn push_end(pending: &mut VecDeque<Record>, view: u32, end: &TermEnd) {
    if let Some(end) = json(end) {
        pending.push_back(Record::TermEnd { view, end });
    }
}

/// A view end the engine decided, as the device sees it.
pub fn term_end(end: ViewEnd) -> TermEnd {
    match end {
        ViewEnd::Closed => TermEnd::Closed {},
        ViewEnd::Stale => TermEnd::Stale {},
        ViewEnd::Overflow => TermEnd::Overflow {},
        ViewEnd::Disconnected => TermEnd::Disconnected {},
        ViewEnd::Shutdown => TermEnd::Shutdown {},
    }
}

fn refusal(r: &InputRefused) -> InputRefusal {
    match r {
        InputRefused::NotAllowed => InputRefusal::NotAllowed,
        InputRefused::Rejected(Rejected::InputHeld) => InputRefusal::InputHeld,
        InputRefused::Rejected(Rejected::Exited) => InputRefusal::Exited,
        InputRefused::Rejected(Rejected::ReadOnly) => InputRefusal::ReadOnly,
        InputRefused::Rejected(Rejected::Detached) => InputRefusal::Detached,
        InputRefused::Rejected(Rejected::Stale { .. }) => InputRefusal::Stale,
        InputRefused::Rejected(_) | InputRefused::NoClipboardRead => InputRefusal::Rejected,
    }
}

/// Records of one message being built, within `limit` bytes.
struct Batch {
    records: Vec<Record>,
    len: usize,
    limit: usize,
}

impl Batch {
    fn new(limit: usize) -> Batch {
        Batch {
            records: Vec::new(),
            len: 0,
            limit,
        }
    }

    fn fits(&self, r: &Record) -> bool {
        self.len + r.encoded_len() <= self.limit
    }

    fn push(&mut self, r: Record) {
        self.len += r.encoded_len();
        self.records.push(r);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn out() -> (Arc<DeviceOut>, Arc<AtomicUsize>) {
        let woken = Arc::new(AtomicUsize::new(0));
        let w = woken.clone();
        let out = DeviceOut::new(
            Arc::new(move || {
                w.fetch_add(1, Ordering::Relaxed);
            }),
            Arc::new(|| {}),
        );
        (out, woken)
    }

    fn request(id: u64, seq: Option<u64>, op: &str) -> Vec<u8> {
        let seq = seq.map_or("null".to_owned(), |s| s.to_string());
        let body = format!(r#"{{"id":{id},"seq":{seq},"op":{op}}}"#);
        api::encode(&[Record::Request(body.into_bytes())]).expect("encode")
    }

    fn replies(msg: &[u8]) -> Vec<Reply> {
        api::decode(msg, api::Origin::Gateway)
            .expect("decode")
            .into_iter()
            .filter_map(|r| match r {
                Record::Reply(b) => Some(serde_json::from_slice(&b).expect("reply")),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn malformed_requests_are_answered_on_the_link_and_garbage_closes() {
        let (out, _) = out();
        let mut rt = Runtime::new(out);
        let now = Instant::now();
        let got = rt
            .receive(&request(1, None, r#"{"op":"teleport"}"#), now)
            .expect("unsupported is answered");
        assert!(got.is_empty());
        rt.receive(
            &request(
                2,
                None,
                r#"{"op":"stop","live":{"key":"k","generation":1}}"#,
            ),
            now,
        )
        .expect("missing seq is answered");
        rt.receive(
            &request(
                3,
                None,
                r#"{"op":"subscribe","feed":{"feed":"sessions","project":"p","limit":0}}"#,
            ),
            now,
        )
        .expect("a broken bound is answered");
        let mut gone = Vec::new();
        let msg = rt.next_message(now, &mut gone).expect("replies");
        let r = replies(&msg);
        assert_eq!(r.len(), 3);
        assert!(matches!(
            r[0].result,
            ReplyResult::Err(ApiError::Unsupported {})
        ));
        assert!(matches!(
            r[1].result,
            ReplyResult::Err(ApiError::BadRequest { .. })
        ));
        assert!(matches!(
            r[2].result,
            ReplyResult::Err(ApiError::BadRequest { .. })
        ));
        let unreadable = api::encode(&[Record::Request(b"[1]".to_vec())]).expect("encode");
        assert_eq!(
            rt.receive(&unreadable, now).err(),
            Some(close::APP_PROTOCOL)
        );
        assert_eq!(
            rt.receive(b"\x10\0\0\0\0", now).err(),
            Some(close::APP_PROTOCOL)
        );
    }

    #[test]
    fn requests_beyond_the_inflight_bound_are_refused_until_answered() {
        let (out, _) = out();
        let mut rt = Runtime::new(out.clone());
        let now = Instant::now();
        for id in 0..u64::from(api::MAX_INFLIGHT) {
            let got = rt
                .receive(&request(id, None, r#"{"op":"ping"}"#), now)
                .expect("ok");
            assert_eq!(got.len(), 1);
        }
        let got = rt
            .receive(&request(99, None, r#"{"op":"ping"}"#), now)
            .expect("ok");
        assert!(got.is_empty(), "the 33rd request stays on the link");
        out.reply(0, ReplyResult::Ok(api::Resp::Pong {}));
        let mut gone = Vec::new();
        let msg = rt.next_message(now, &mut gone).expect("message");
        let r = replies(&msg);
        assert!(matches!(
            r[0].result,
            ReplyResult::Err(ApiError::TooManyRequests {})
        ));
        assert_eq!(r[1].id, 0);
        let got = rt
            .receive(&request(100, None, r#"{"op":"ping"}"#), now)
            .expect("ok");
        assert_eq!(got.len(), 1, "an answered request frees a slot");
    }

    #[test]
    fn the_window_bounds_unacknowledged_messages_and_a_silent_device_stalls() {
        let (out, _) = out();
        let mut rt = Runtime::new(out.clone());
        let start = Instant::now();
        let mut gone = Vec::new();
        let mut sent = 0;
        for i in 0..100 {
            out.reply(i, ReplyResult::Ok(api::Resp::Pong {}));
            if rt.next_message(start, &mut gone).is_some() {
                sent += 1;
            }
        }
        assert_eq!(sent, api::WINDOW_MESSAGES as usize);
        assert!(!rt.stalled(start));
        assert!(rt.stalled(start + rt.stall));
        let ack = api::encode(&[Record::Ack { messages: 32 }]).expect("encode");
        rt.receive(&ack, start + Duration::from_secs(1))
            .expect("ack");
        assert!(!rt.stalled(start + rt.stall), "an ack restarts the clock");
        assert!(
            rt.next_message(start, &mut gone).is_some(),
            "the window reopened"
        );
        let too_many = api::encode(&[Record::Ack { messages: 1000 }]).expect("encode");
        assert_eq!(
            rt.receive(&too_many, start).err(),
            Some(close::APP_PROTOCOL)
        );
    }

    #[test]
    fn a_full_event_queue_turns_into_one_resync_and_feeds_keep_only_the_latest_value() {
        let (out, _) = out();
        for _ in 0..api::EVENT_QUEUE + 3 {
            out.event(&Event::Resync {});
        }
        assert!(lock(&out.q).events.len() <= api::EVENT_QUEUE);
        let feed = Feed::Live {};
        out.feed(&feed, b"{\"a\":1}".to_vec());
        out.feed(&feed, b"{\"a\":2}".to_vec());
        assert!(out.feed_pending(&feed));
        let q = lock(&out.q);
        assert_eq!(q.feeds.len(), 1);
        assert_eq!(q.feeds[0].1, b"{\"a\":2}");
    }

    #[test]
    fn a_reply_that_does_not_fit_one_record_becomes_too_large() {
        let huge = api::Resp::Diff {
            diff: api::Diff {
                text: "x".repeat(MAX_REPLY),
                binary: false,
                truncated: false,
                added: 0,
                removed: 0,
            },
        };
        let body = reply_body(5, ReplyResult::Ok(huge)).expect("body");
        let r: Reply = serde_json::from_slice(&body).expect("reply");
        assert!(matches!(r.result, ReplyResult::Err(ApiError::TooLarge {})));
    }

    fn attach(out: &DeviceOut, id: u32, engine: u64, mode: ViewMode) {
        out.attach(NewView {
            id,
            engine,
            side: Arc::new(ViewSide::polled(Arc::new(|| {}))),
            mode,
        });
    }

    fn input(view: u32, ctrl_c: bool, bytes: &[u8]) -> Record {
        Record::Input {
            view,
            ctrl_c,
            bytes: bytes.to_vec(),
        }
    }

    /// The `input_refused` events queued so far (taken).
    fn refusals(out: &DeviceOut) -> Vec<(u32, InputRefusal)> {
        let mut q = lock(&out.q);
        q.event_bytes = 0;
        q.events
            .drain(..)
            .filter_map(|b| match serde_json::from_slice(&b).expect("event") {
                Event::InputRefused { view, reason } => Some((view, reason)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_device_resize_is_clamped_to_a_sane_terminal() {
        let (out, _) = out();
        let mut rt = Runtime::new(out.clone());
        attach(&out, 3, 30, ViewMode::Control);
        for ((cols, rows), want) in [
            ((u16::MAX, u16::MAX), (MAX_COLS, MAX_ROWS)),
            ((0, 0), (1, 1)),
            ((120, 40), (120, 40)),
        ] {
            let msg = api::encode(&[Record::Resize {
                view: 3,
                cols,
                rows,
            }])
            .expect("encode");
            let got = rt.receive(&msg, Instant::now()).expect("admitted");
            let [Inbound::Resize { cols, rows, .. }] = &got[..] else {
                panic!("{got:?}");
            };
            assert_eq!((*cols, *rows), want);
        }
    }

    #[test]
    fn terminal_input_is_admitted_by_view_mode_and_coalesced_per_view() {
        let (out, _) = out();
        let mut rt = Runtime::new(out.clone());
        let now = Instant::now();
        attach(&out, 1, 10, ViewMode::Observe);
        attach(&out, 2, 20, ViewMode::Input);
        attach(&out, 3, 30, ViewMode::Control);
        let msg = api::encode(&[
            input(1, false, b"a"),
            input(1, false, b"b"),
            input(2, false, b""),
            input(2, false, b"ab"),
            input(2, false, b"cd"),
            input(2, true, b""),
            input(2, false, b"e"),
            input(9, false, b"z"),
            Record::Resize {
                view: 2,
                cols: 10,
                rows: 10,
            },
            Record::Resize {
                view: 3,
                cols: 80,
                rows: 24,
            },
            Record::Resize {
                view: 3,
                cols: 100,
                rows: 30,
            },
        ])
        .expect("encode");
        let got = rt.receive(&msg, now).expect("admitted");
        let shape: Vec<String> = got
            .iter()
            .map(|i| match i {
                Inbound::Input {
                    view,
                    bytes,
                    ctrl_c,
                    ..
                } => format!("in {view} {} {ctrl_c}", String::from_utf8_lossy(bytes)),
                Inbound::Resize {
                    view, cols, rows, ..
                } => format!("resize {view} {cols}x{rows}"),
                other => panic!("unexpected {other:?}"),
            })
            .collect();
        assert_eq!(
            shape,
            [
                "in 20 abcd false",
                "in 20  true",
                "in 20 e false",
                "resize 30 100x30"
            ]
        );
        assert_eq!(
            refusals(&out),
            [(1, InputRefusal::NotAllowed), (9, InputRefusal::Detached)],
            "one refusal per view and message"
        );
        assert_eq!(rt.backlog.load(Ordering::Acquire), 4 * RECORD_COST + 5);
        drop(got);
        assert_eq!(
            rt.backlog.load(Ordering::Acquire),
            0,
            "processed input frees it"
        );
    }

    #[test]
    fn input_beyond_the_terminal_backlog_is_refused_until_the_engine_catches_up() {
        let (out, _) = out();
        let mut rt = Runtime::new(out.clone());
        let now = Instant::now();
        attach(&out, 1, 10, ViewMode::Control);
        let chunk = vec![b'x'; api::MAX_INPUT];
        let fit = TERMINAL_BACKLOG / (api::MAX_INPUT + RECORD_COST);
        let mut held = Vec::new();
        for _ in 0..fit {
            let msg = api::encode(&[input(1, false, &chunk)]).expect("encode");
            held.extend(rt.receive(&msg, now).expect("ok"));
        }
        assert_eq!(held.len(), fit);
        assert!(refusals(&out).is_empty());
        let msg = api::encode(&[input(1, false, &chunk)]).expect("encode");
        assert!(rt.receive(&msg, now).expect("ok").is_empty());
        assert_eq!(refusals(&out), [(1, InputRefusal::Rejected)]);
        // A small ctrl-c still fits; the full chunk after it does not.
        let msg = api::encode(&[input(1, true, b""), input(1, false, &chunk)]).expect("encode");
        let got = rt.receive(&msg, now).expect("ok");
        assert_eq!(got.len(), 1);
        assert_eq!(refusals(&out), [(1, InputRefusal::Rejected)]);
        drop(got);
        held.clear();
        let msg = api::encode(&[input(1, false, &chunk)]).expect("encode");
        assert_eq!(rt.receive(&msg, now).expect("ok").len(), 1, "room again");
        assert!(refusals(&out).is_empty());
    }

    #[test]
    fn a_device_piling_up_local_replies_is_closed() {
        let now = Instant::now();
        let unsupported = request(1, None, r#"{"op":"teleport"}"#);
        let mut rt = Runtime::new(out().0);
        for _ in 0..LOCAL_REPLIES {
            rt.receive(&unsupported, now).expect("held");
        }
        assert_eq!(
            rt.receive(&unsupported, now).err(),
            Some(close::APP_PROTOCOL)
        );
        // Sending them frees the room.
        let mut rt = Runtime::new(out().0);
        for _ in 0..LOCAL_REPLIES {
            rt.receive(&unsupported, now).expect("held");
        }
        let mut gone = Vec::new();
        assert!(rt.next_message(now, &mut gone).is_some());
        assert_eq!(rt.local_bytes, 0);
        rt.receive(&unsupported, now).expect("room after sending");
    }

    #[test]
    fn a_message_never_exceeds_the_remaining_window_bytes() {
        let (out, _) = out();
        let mut rt = Runtime::new(out.clone());
        let now = Instant::now();
        for id in 0..6 {
            out.reply(
                id,
                ReplyResult::Ok(api::Resp::Diff {
                    diff: api::Diff {
                        text: "x".repeat(300 * 1024),
                        binary: false,
                        truncated: false,
                        added: 0,
                        removed: 0,
                    },
                }),
            );
        }
        let mut gone = Vec::new();
        let first = rt.next_message(now, &mut gone).expect("first");
        assert_eq!(replies(&first).len(), 3);
        assert!(
            rt.next_message(now, &mut gone).is_none(),
            "a 300 KiB reply does not fit the remaining credit"
        );
        assert!(rt.unacked_bytes <= api::WINDOW_BYTES as usize);
        let ack = api::encode(&[Record::Ack { messages: 1 }]).expect("encode");
        rt.receive(&ack, now).expect("ack");
        let second = rt.next_message(now, &mut gone).expect("second");
        assert_eq!(replies(&second).len(), 3);
    }

    #[test]
    fn the_welcome_goes_before_the_links_own_replies() {
        let (out, _) = out();
        let mut rt = Runtime::new(out.clone());
        let now = Instant::now();
        rt.receive(&request(1, None, r#"{"op":"teleport"}"#), now)
            .expect("answered");
        lock(&out.q)
            .control
            .push_back(Record::Welcome(b"{}".to_vec()));
        let mut gone = Vec::new();
        let msg = rt.next_message(now, &mut gone).expect("message");
        let records = api::decode(&msg, api::Origin::Gateway).expect("decode");
        assert!(matches!(records[0], Record::Welcome(_)), "{records:?}");
        assert!(matches!(records[1], Record::Reply(_)));
    }

    #[test]
    fn an_engine_end_after_a_link_end_keeps_the_queued_t_end() {
        let (out, _) = out();
        let mut rt = Runtime::new(out.clone());
        let now = Instant::now();
        attach(&out, 1, 10, ViewMode::Control);
        rt.sync_views();
        let v = rt.views.get_mut(&1).expect("view");
        v.ended = true;
        push_end(&mut v.pending, 1, &TermEnd::Overflow {});
        out.end_view(1, TermEnd::Detached {});
        let mut gone = Vec::new();
        let msg = rt.next_message(now, &mut gone).expect("message");
        let ends: Vec<TermEnd> = api::decode(&msg, api::Origin::Gateway)
            .expect("decode")
            .into_iter()
            .filter_map(|r| match r {
                Record::TermEnd { view: 1, end } => {
                    Some(serde_json::from_slice(&end).expect("end"))
                }
                _ => None,
            })
            .collect();
        assert_eq!(ends, [TermEnd::Overflow {}]);
        assert_eq!(gone, [10]);
    }

    #[test]
    fn input_to_many_unknown_views_is_refused_once_per_message() {
        let (out, _) = out();
        let mut rt = Runtime::new(out.clone());
        attach(&out, 1, 10, ViewMode::Observe);
        let mut records = vec![input(1, false, b"x")];
        let mut len = records[0].encoded_len();
        let mut id = 1_000;
        loop {
            let r = input(id, false, b"x");
            if len + r.encoded_len() > MAX_APP_MESSAGE {
                break;
            }
            len += r.encoded_len();
            records.push(r);
            id += 1;
        }
        assert!(records.len() > 50_000, "{}", records.len());
        let msg = api::encode(&records).expect("encode");
        let start = Instant::now();
        assert!(rt.receive(&msg, start).expect("ok").is_empty());
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "{:?}",
            start.elapsed()
        );
        assert_eq!(
            refusals(&out),
            [
                (1, InputRefusal::NotAllowed),
                (1_000, InputRefusal::Detached)
            ]
        );
    }

    #[test]
    fn an_ended_view_lets_go_of_its_terminal_and_its_end_does_not_wait_behind_replies() {
        let (out, _) = out();
        let mut rt = Runtime::new(out.clone());
        let now = Instant::now();
        let side = Arc::new(ViewSide::polled(Arc::new(|| {})));
        out.attach(NewView {
            id: 1,
            engine: 10,
            side: side.clone(),
            mode: ViewMode::Control,
        });
        rt.sync_views();
        assert_eq!(Arc::strong_count(&side), 2);
        out.end_view(1, TermEnd::Detached {});
        rt.sync_views();
        assert_eq!(
            Arc::strong_count(&side),
            1,
            "the terminal is let go at once"
        );
        assert_eq!(lock(&out.q).retained, 1, "the T_END is still owed");
        // Two replies that each fill most of a message.
        for id in 0..2 {
            out.reply(
                id,
                ReplyResult::Ok(api::Resp::Diff {
                    diff: api::Diff {
                        text: "x".repeat(600 * 1024),
                        binary: false,
                        truncated: false,
                        added: 0,
                        removed: 0,
                    },
                }),
            );
        }
        let mut gone = Vec::new();
        let msg = rt.next_message(now, &mut gone).expect("message");
        let records = api::decode(&msg, api::Origin::Gateway).expect("decode");
        assert!(
            matches!(records[0], Record::TermEnd { view: 1, .. }),
            "the end goes first: {:?}",
            records.iter().map(Record::kind).collect::<Vec<_>>()
        );
        assert_eq!(gone, [10]);
        assert!(rt.views.is_empty());
        assert_eq!(lock(&out.q).retained, 0);
    }

    #[test]
    fn attaches_wait_while_the_link_still_holds_too_many_views() {
        let (out, _) = out();
        let mut rt = Runtime::new(out.clone());
        let now = Instant::now();
        for id in 1..=RETAINED_VIEWS as u32 {
            assert!(out.view_room());
            attach(&out, id, u64::from(id), ViewMode::Observe);
            out.end_view(id, TermEnd::Detached {});
        }
        assert!(
            !out.view_room(),
            "ended views count until their T_END is sent"
        );
        let mut gone = Vec::new();
        rt.next_message(now, &mut gone).expect("the ends");
        assert_eq!(gone.len(), RETAINED_VIEWS);
        assert!(out.view_room());
    }
}
