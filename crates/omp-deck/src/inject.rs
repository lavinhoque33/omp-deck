//! Per-tab command injection into omp's composer (docs/notes/omp-actions.md §3.7).
//!
//! A pure state machine driven from the frame loop: [`Injector::poll`] takes the current time and
//! the tab's observable state and returns the bytes to write now plus when to look again. It never
//! sleeps and never touches the PTY itself, so the whole protocol is testable with a fake clock.
//!
//! Slash command sequence: (the cursor must be in omp's composer; a draft there → confirm →
//! `0x03`, ≥ 650 ms after the last Ctrl+C, then wait 650 ms) → bracketed paste of the command →
//! 150 ms → (idle only) `Esc` to close an autocomplete popup → 150 ms → `\r`. Direct hotkeys are a
//! single write. Stop is `Esc`, then — only if omp still works after 650 ms — one more `Esc`.
//! Jobs of one tab are strictly serialized with a pause between them.
//!
//! Safety rules: a job that started while omp was idle is abandoned (no further bytes) as soon as
//! omp is no longer idle, so a turn that begins mid-sequence is never interrupted by our `Esc` or
//! submitted by our `\r`; the caller drops the user's own keystrokes while a job is in flight
//! ([`Poll::hold_for`]); no byte follows a lone `Esc` within 100 ms (omp would merge them).

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use deck_term::ScreenText;

use crate::composer::{self, Composer};
use crate::statusfmt::Activity;

/// omp exits on two Ctrl+C within 500 ms; keep clear of that.
pub const CTRL_C_GAP: Duration = Duration::from_millis(650);
/// Time omp needs to clear the composer after Ctrl+C.
const AFTER_CLEAR: Duration = Duration::from_millis(650);
const AFTER_PASTE: Duration = Duration::from_millis(150);
const AFTER_ESC: Duration = Duration::from_millis(150);
/// Stop: how long the first `Esc` gets to end the turn before a second one is sent.
const STOP_RETRY: Duration = Duration::from_millis(650);
/// Pause between two jobs of the same tab.
const BETWEEN_JOBS: Duration = Duration::from_millis(300);
/// User input stays dropped this long after the last injected byte.
pub const HOLD_TAIL: Duration = Duration::from_millis(150);

const CTRL_C: u8 = 0x03;
const ESC: u8 = 0x1b;
const PASTE_START: &[u8] = b"\x1b[200~";
const PASTE_END: &[u8] = b"\x1b[201~";

/// What omp must be doing for a job to run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Requires {
    Any,
    /// `Activity::Idle` exactly (not attention, not unknown, not busy): `/compact` would abort a
    /// turn, `/handoff` refuses, and omp's title must prove it is waiting for input.
    Idle,
    /// Only meaningful while a turn runs (stop).
    Busy,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Payload {
    /// A slash command typed through the composer (single line, no control characters).
    Slash(String),
    /// Raw bytes (hotkeys such as `ESC P`); no composer handling needed.
    Keys(Vec<u8>),
    /// `Esc`, and a second `Esc` after [`STOP_RETRY`] if the turn still runs.
    Stop,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Job {
    /// Human label for messages ("Compact", "Switch model").
    pub label: String,
    pub payload: Payload,
    pub requires: Requires,
}

/// One line without control characters: whitespace controls (newline, tab, NEL …) become a single
/// blank, every other C0/C1 control (ESC, DEL, …) is dropped. An `ESC[201~` inside a pasted title
/// would otherwise end the bracketed paste early and run the rest as keystrokes.
pub fn sanitize_line(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_whitespace() {
            if !out.ends_with(' ') {
                out.push(' ');
            }
        } else if !c.is_control() {
            out.push(c);
        }
    }
    out.trim().to_owned()
}

impl Job {
    /// The text is sanitized with [`sanitize_line`], whatever the caller built it from.
    pub fn slash(label: impl Into<String>, text: &str, requires: Requires) -> Job {
        Job {
            label: label.into(),
            payload: Payload::Slash(sanitize_line(text)),
            requires,
        }
    }

    pub fn keys(label: impl Into<String>, bytes: &[u8], requires: Requires) -> Job {
        Job {
            label: label.into(),
            payload: Payload::Keys(bytes.to_vec()),
            requires,
        }
    }

    pub fn stop() -> Job {
        Job {
            label: "Stop".to_owned(),
            payload: Payload::Stop,
            requires: Requires::Busy,
        }
    }
}

/// Observable tab state handed to [`Injector::poll`].
pub struct Env<'a> {
    pub now: Instant,
    /// From the terminal title; idle-only checks need `Activity::Idle` exactly.
    pub activity: Activity,
    /// Last 0x03 written to this tab's PTY (by the user or by us).
    pub last_ctrl_c: Option<Instant>,
    /// The visible grid; only inspected when a slash job starts.
    pub screen: &'a dyn Fn() -> ScreenText,
}

/// What the caller must do after a poll.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Poll {
    /// Chunks to write to the PTY, in order.
    pub writes: Vec<Vec<u8>>,
    /// Poll again no later than this from now (`None` = nothing pending).
    pub wake_in: Option<Duration>,
    /// The composer holds an unsent draft: ask the user, then call [`Injector::answer`].
    pub confirm: Option<Confirm>,
    /// Jobs dropped because their precondition failed (`Requires`), with the label.
    pub refused: Vec<(String, Requires)>,
    /// Slash jobs dropped because the cursor is not in omp's composer (an overlay is open).
    pub dialog_open: Vec<String>,
    /// Jobs abandoned after they started because omp stopped being idle.
    pub aborted: Vec<String>,
    /// Drop the user's keyboard/paste input to the terminal for this long from now: bytes were
    /// written, or a job with written bytes still waits for its next step.
    pub hold_for: Option<Duration>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Confirm {
    pub label: String,
    pub draft: String,
}

#[derive(Debug)]
enum Stage {
    /// Look at the composer first.
    Start,
    /// Waiting for the user to decide about the draft.
    AwaitConfirm,
    /// Waiting until a Ctrl+C is safe.
    ClearPending,
    /// Ctrl+C sent; composer clearing until this instant.
    Cleared(Instant),
    Pasted(Instant),
    /// Slash: `Esc` sent (or skipped), `\r` due at this instant. Stop: second-`Esc` check.
    Escaped(Instant),
}

struct Active {
    job: Job,
    stage: Stage,
    /// A byte of this job already reached the PTY.
    wrote: bool,
    /// omp was idle when the job started: it must stay idle until the final Enter.
    started_idle: bool,
}

#[derive(Default)]
pub struct Injector {
    queue: VecDeque<Job>,
    active: Option<Active>,
    /// Earliest start of the next job.
    ready_at: Option<Instant>,
    /// Own record of the last Ctrl+C (also covers PTY writes not yet seen by the terminal).
    ctrl_c: Option<Instant>,
    /// When the last byte of any job was handed to the PTY.
    last_write: Option<Instant>,
}

impl Injector {
    pub fn push(&mut self, job: Job) {
        self.queue.push_back(job);
    }

    /// True while a job runs or is queued.
    pub fn is_busy(&self) -> bool {
        self.active.is_some() || !self.queue.is_empty()
    }

    /// True while a confirmation from the user is outstanding.
    pub fn awaiting_answer(&self) -> bool {
        matches!(
            self.active,
            Some(Active {
                stage: Stage::AwaitConfirm,
                ..
            })
        )
    }

    /// The user's answer to [`Poll::confirm`]: `true` = clear the draft and go on, `false` = drop the job.
    pub fn answer(&mut self, clear: bool) {
        if !self.awaiting_answer() {
            return;
        }
        if clear {
            if let Some(a) = &mut self.active {
                a.stage = Stage::ClearPending;
            }
        } else {
            self.active = None;
        }
    }

    /// Drop everything (omp exited).
    pub fn clear(&mut self) {
        self.queue.clear();
        self.active = None;
    }

    /// Tab closing: drop every job that has not put a byte into the PTY yet. True when nothing is
    /// left, i.e. the injector is at a job boundary and the tab can go now.
    pub fn drop_unstarted(&mut self) -> bool {
        self.queue.clear();
        if self.active.as_ref().is_some_and(|a| !a.wrote) {
            self.active = None;
        }
        self.active.is_none()
    }

    /// Tab closing: true once nothing is left to send, no job is mid-way and the last written byte
    /// is at least [`HOLD_TAIL`] old (the terminal needs that long to deliver it before detaching).
    pub fn can_close(&mut self, now: Instant) -> bool {
        self.drop_unstarted()
            && self
                .last_write
                .is_none_or(|t| now.saturating_duration_since(t) >= HOLD_TAIL)
    }

    pub fn poll(&mut self, env: &Env<'_>) -> Poll {
        let mut out = Poll::default();
        // Several steps can complete in one poll (e.g. a zero wait), so loop until blocked.
        loop {
            if self.active.is_none() && !self.start_next(env, &mut out) {
                break;
            }
            let Some(active) = self.active.as_mut() else {
                break;
            };
            match step(active, env, &mut self.ctrl_c, &mut out) {
                Step::Continue => {}
                Step::Wait(d) => {
                    out.wake_in = Some(d);
                    break;
                }
                Step::Blocked => break,
                Step::Done => {
                    self.active = None;
                    self.ready_at = Some(env.now + BETWEEN_JOBS);
                }
                Step::Dropped => {
                    // Nothing was sent, so no pause is needed before the next job.
                    self.active = None;
                }
                Step::Aborted => {
                    if let Some(a) = self.active.take() {
                        out.aborted.push(a.job.label);
                    }
                    self.ready_at = Some(env.now + BETWEEN_JOBS);
                }
            }
        }
        if !out.writes.is_empty() {
            self.last_write = Some(env.now);
        }
        out.hold_for = match &self.active {
            Some(a) if a.wrote => Some(out.wake_in.unwrap_or_default() + HOLD_TAIL),
            None if !out.writes.is_empty() => Some(HOLD_TAIL),
            _ => None,
        };
        out
    }

    /// Pops the next runnable job into `active`; false when nothing can start now.
    fn start_next(&mut self, env: &Env<'_>, out: &mut Poll) -> bool {
        if let Some(at) = self.ready_at {
            if env.now < at {
                if !self.queue.is_empty() {
                    out.wake_in = Some(at - env.now);
                }
                return false;
            }
            self.ready_at = None;
        }
        while let Some(job) = self.queue.pop_front() {
            let blocked = match job.requires {
                Requires::Any => false,
                Requires::Idle => env.activity != Activity::Idle,
                Requires::Busy => !env.activity.is_busy(),
            };
            if blocked {
                out.refused.push((job.label, job.requires));
                continue;
            }
            self.active = Some(Active {
                job,
                stage: Stage::Start,
                wrote: false,
                started_idle: env.activity == Activity::Idle,
            });
            return true;
        }
        false
    }
}

enum Step {
    Continue,
    Wait(Duration),
    /// Needs outside input (confirmation).
    Blocked,
    Done,
    /// Ended before any byte was written.
    Dropped,
    /// Ended without its final Enter because omp left the idle state.
    Aborted,
}

fn write(a: &mut Active, out: &mut Poll, bytes: Vec<u8>) {
    a.wrote = true;
    out.writes.push(bytes);
}

fn step(a: &mut Active, env: &Env<'_>, ctrl_c: &mut Option<Instant>, out: &mut Poll) -> Step {
    let now = env.now;
    let wait_until = |t: Instant| Step::Wait(t.saturating_duration_since(now));
    if a.started_idle && env.activity != Activity::Idle && !matches!(a.stage, Stage::Start) {
        return Step::Aborted;
    }
    match (&a.job.payload, &a.stage) {
        (Payload::Keys(bytes), Stage::Start) => {
            let bytes = bytes.clone();
            write(a, out, bytes);
            Step::Done
        }
        (Payload::Keys(_), _) => Step::Done,
        (Payload::Stop, Stage::Start) => {
            write(a, out, vec![ESC]);
            a.stage = Stage::Escaped(now + STOP_RETRY);
            wait_until(now + STOP_RETRY)
        }
        (Payload::Stop, Stage::Escaped(until)) => {
            if now < *until {
                return wait_until(*until);
            }
            // Never two Esc within 500 ms; by now the first one was ≥ 650 ms ago.
            if env.activity.is_busy() {
                write(a, out, vec![ESC]);
            }
            Step::Done
        }
        (Payload::Stop, _) => Step::Done,
        (Payload::Slash(_), Stage::Start) => match composer::inspect(&(env.screen)()) {
            Composer::Missing => {
                out.dialog_open.push(a.job.label.clone());
                Step::Dropped
            }
            Composer::Draft(draft) => {
                out.confirm = Some(Confirm {
                    label: a.job.label.clone(),
                    draft,
                });
                a.stage = Stage::AwaitConfirm;
                Step::Blocked
            }
            Composer::Empty => {
                a.stage = Stage::Cleared(now);
                Step::Continue
            }
        },
        (Payload::Slash(_), Stage::AwaitConfirm) => Step::Blocked,
        (Payload::Slash(_), Stage::ClearPending) => {
            let last = [*ctrl_c, env.last_ctrl_c].into_iter().flatten().max();
            match last {
                Some(t) if now < t + CTRL_C_GAP => wait_until(t + CTRL_C_GAP),
                _ => {
                    write(a, out, vec![CTRL_C]);
                    *ctrl_c = Some(now);
                    a.stage = Stage::Cleared(now + AFTER_CLEAR);
                    wait_until(now + AFTER_CLEAR)
                }
            }
        }
        (Payload::Slash(text), Stage::Cleared(until)) => {
            if now < *until {
                return wait_until(*until);
            }
            let mut buf = Vec::with_capacity(text.len() + 12);
            buf.extend_from_slice(PASTE_START);
            buf.extend_from_slice(text.as_bytes());
            buf.extend_from_slice(PASTE_END);
            write(a, out, buf);
            a.stage = Stage::Pasted(now + AFTER_PASTE);
            wait_until(now + AFTER_PASTE)
        }
        (Payload::Slash(_), Stage::Pasted(until)) => {
            if now < *until {
                return wait_until(*until);
            }
            if env.activity == Activity::Idle {
                write(a, out, vec![ESC]);
                a.stage = Stage::Escaped(now + AFTER_ESC);
            } else {
                // Esc would interrupt a turn; busy-capable commands run without the popup step.
                a.stage = Stage::Escaped(now);
            }
            Step::Continue
        }
        (Payload::Slash(_), Stage::Escaped(until)) => {
            if now < *until {
                return wait_until(*until);
            }
            write(a, out, vec![b'\r']);
            Step::Done
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Rail composer with `row` text, cursor at its end, as captured on omp 18.12.5.
    fn rail(row: &str) -> ScreenText {
        let status = " π · ◒ Fake One · 🗑 w3f/proj · ◫ 7.4%/100K ⟲".to_owned();
        let composer = format!("▎ {row}").trim_end().to_owned();
        let col = composer.chars().count().max(2);
        ScreenText {
            lines: vec!["".into(), "  v18.12.5".into(), composer, "".into(), status],
            cursor_row: 2,
            cursor_col: col,
        }
    }

    fn overlay() -> ScreenText {
        ScreenText {
            lines: vec![
                "".into(),
                "▎".into(),
                "╭─ Switch Model ───────╮".into(),
                "│  🔍 >                │".into(),
                "╰──────────────────────╯".into(),
            ],
            cursor_row: 0,
            cursor_col: 0,
        }
    }

    struct Fixture {
        base: Instant,
        screen: ScreenText,
        activity: Activity,
        last_ctrl_c: Option<Instant>,
    }

    impl Fixture {
        fn new() -> Fixture {
            Fixture {
                base: Instant::now(),
                screen: rail(""),
                activity: Activity::Idle,
                last_ctrl_c: None,
            }
        }
        fn poll(&self, inj: &mut Injector, ms: u64) -> Poll {
            let screen = self.screen.clone();
            let f = move || screen.clone();
            inj.poll(&Env {
                now: self.base + Duration::from_millis(ms),
                activity: self.activity,
                last_ctrl_c: self.last_ctrl_c,
                screen: &f,
            })
        }
    }

    fn drain(fx: &Fixture, inj: &mut Injector) -> Vec<(u64, Vec<u8>)> {
        let mut writes = Vec::new();
        let mut t = 0;
        while inj.is_busy() && t < 10_000 {
            let p = fx.poll(inj, t);
            for w in p.writes {
                writes.push((t, w));
            }
            t += 10;
        }
        writes
    }

    #[test]
    fn slash_without_draft_pastes_escapes_then_enters() {
        let fx = Fixture::new();
        let mut inj = Injector::default();
        inj.push(Job::slash("Context", "/context", Requires::Any));
        let w = drain(&fx, &mut inj);
        assert_eq!(
            w,
            vec![
                (0, b"\x1b[200~/context\x1b[201~".to_vec()),
                (150, vec![0x1b]),
                (300, vec![b'\r']),
            ]
        );
    }

    #[test]
    fn busy_slash_skips_escape() {
        let mut fx = Fixture::new();
        fx.activity = Activity::Busy;
        let mut inj = Injector::default();
        inj.push(Job::slash("Switch", "/effort low", Requires::Any));
        let w = drain(&fx, &mut inj);
        let bytes: Vec<_> = w.iter().map(|(_, b)| b.clone()).collect();
        assert_eq!(
            bytes,
            vec![b"\x1b[200~/effort low\x1b[201~".to_vec(), vec![b'\r']]
        );
        assert_eq!(w[1].0, 150);
    }

    #[test]
    fn attention_and_unknown_slash_skip_escape_too() {
        for activity in [Activity::Attention, Activity::Unknown] {
            let mut fx = Fixture::new();
            fx.activity = activity;
            let mut inj = Injector::default();
            inj.push(Job::slash("Context", "/context", Requires::Any));
            let w = drain(&fx, &mut inj);
            assert!(w.iter().all(|(_, b)| b != &vec![0x1b]), "{activity:?}");
            assert_eq!(w.len(), 2);
        }
    }

    #[test]
    fn draft_asks_then_clears_with_ctrl_c_and_waits() {
        let mut fx = Fixture::new();
        fx.screen = rail("half typed");
        let mut inj = Injector::default();
        inj.push(Job::slash(
            "Compact (soft)",
            "/compact soft",
            Requires::Idle,
        ));
        let p = fx.poll(&mut inj, 0);
        assert_eq!(
            p.confirm,
            Some(Confirm {
                label: "Compact (soft)".to_owned(),
                draft: "half typed".to_owned()
            })
        );
        assert!(p.writes.is_empty() && inj.awaiting_answer());
        // Nothing happens until the user answers.
        assert!(fx.poll(&mut inj, 5_000).writes.is_empty());
        inj.answer(true);
        let p = fx.poll(&mut inj, 5_000);
        assert_eq!(p.writes, vec![vec![0x03]]);
        assert_eq!(p.wake_in, Some(Duration::from_millis(650)));
        assert!(fx.poll(&mut inj, 5_649).writes.is_empty());
        let p = fx.poll(&mut inj, 5_650);
        assert_eq!(p.writes, vec![b"\x1b[200~/compact soft\x1b[201~".to_vec()]);
    }

    #[test]
    fn multiline_draft_with_cursor_on_empty_last_line_asks() {
        let mut fx = Fixture::new();
        fx.screen = ScreenText {
            lines: vec!["".into(), "▎ one".into(), "▎ two".into(), "▎".into()],
            cursor_row: 3,
            cursor_col: 2,
        };
        let mut inj = Injector::default();
        inj.push(Job::slash("Handoff", "/handoff", Requires::Idle));
        let p = fx.poll(&mut inj, 0);
        assert_eq!(p.confirm.map(|c| c.draft), Some("one ↵ two".to_owned()));
    }

    #[test]
    fn declined_draft_drops_the_job() {
        let mut fx = Fixture::new();
        fx.screen = rail("hello");
        let mut inj = Injector::default();
        inj.push(Job::slash("Handoff", "/handoff", Requires::Idle));
        fx.poll(&mut inj, 0);
        inj.answer(false);
        assert!(!inj.is_busy());
        assert!(fx.poll(&mut inj, 10).writes.is_empty());
    }

    #[test]
    fn overlay_refuses_slash_but_not_hotkeys() {
        let mut fx = Fixture::new();
        fx.screen = overlay();
        let mut inj = Injector::default();
        inj.push(Job::slash("Compact", "/compact", Requires::Idle));
        inj.push(Job::keys("Plan mode", b"\x1bP", Requires::Any));
        let p = fx.poll(&mut inj, 0);
        assert_eq!(p.dialog_open, vec!["Compact".to_owned()]);
        // The refusal costs no pause: the hotkey runs in the same poll.
        assert_eq!(p.writes, vec![b"\x1bP".to_vec()]);
    }

    #[test]
    fn ctrl_c_respects_users_recent_ctrl_c() {
        let mut fx = Fixture::new();
        fx.screen = rail("draft");
        fx.last_ctrl_c = Some(fx.base + Duration::from_millis(100));
        let mut inj = Injector::default();
        inj.push(Job::slash("Compact", "/compact", Requires::Any));
        fx.poll(&mut inj, 200);
        inj.answer(true);
        let p = fx.poll(&mut inj, 200);
        assert!(p.writes.is_empty());
        assert_eq!(p.wake_in, Some(Duration::from_millis(550)));
        let p = fx.poll(&mut inj, 750);
        assert_eq!(p.writes, vec![vec![0x03]]);
    }

    #[test]
    fn idle_only_needs_activity_idle_exactly() {
        for activity in [Activity::Busy, Activity::Attention, Activity::Unknown] {
            let mut fx = Fixture::new();
            fx.activity = activity;
            let mut inj = Injector::default();
            inj.push(Job::slash("Compact", "/compact", Requires::Idle));
            let p = fx.poll(&mut inj, 0);
            assert_eq!(
                p.refused,
                vec![("Compact".to_owned(), Requires::Idle)],
                "{activity:?}"
            );
            assert!(p.writes.is_empty());
        }
    }

    #[test]
    fn busy_job_refused_while_not_busy() {
        let mut fx = Fixture::new();
        let mut inj = Injector::default();
        inj.push(Job::stop());
        let p = fx.poll(&mut inj, 0);
        assert_eq!(p.refused, vec![("Stop".to_owned(), Requires::Busy)]);
        fx.activity = Activity::Busy;
        inj.push(Job::stop());
        assert_eq!(fx.poll(&mut inj, 1_000).writes, vec![vec![0x1b]]);
    }

    #[test]
    fn leaving_idle_before_the_enter_aborts_without_more_bytes() {
        let mut fx = Fixture::new();
        let mut inj = Injector::default();
        inj.push(Job::slash("Compact", "/compact", Requires::Idle));
        let p = fx.poll(&mut inj, 0);
        assert_eq!(p.writes.len(), 1, "paste goes out");
        // A queued message starts a turn while the paste waits: no Esc (would interrupt) and no Enter.
        fx.activity = Activity::Busy;
        let p = fx.poll(&mut inj, 150);
        assert!(p.writes.is_empty());
        assert_eq!(p.aborted, vec!["Compact".to_owned()]);
        assert!(!inj.is_busy());
        assert!(drain(&fx, &mut inj).is_empty());
    }

    #[test]
    fn leaving_idle_between_escape_and_enter_aborts() {
        let mut fx = Fixture::new();
        let mut inj = Injector::default();
        inj.push(Job::slash("Context", "/context", Requires::Any));
        fx.poll(&mut inj, 0);
        assert_eq!(fx.poll(&mut inj, 150).writes, vec![vec![0x1b]]);
        fx.activity = Activity::Attention;
        let p = fx.poll(&mut inj, 300);
        assert!(p.writes.is_empty());
        assert_eq!(p.aborted, vec!["Context".to_owned()]);
    }

    #[test]
    fn a_job_that_started_busy_runs_to_its_enter() {
        let mut fx = Fixture::new();
        fx.activity = Activity::Busy;
        let mut inj = Injector::default();
        inj.push(Job::slash("Switch", "/switch a/b", Requires::Any));
        fx.poll(&mut inj, 0);
        fx.activity = Activity::Idle; // the turn ended meanwhile: not an abort
        let p = fx.poll(&mut inj, 150);
        assert!(p.aborted.is_empty());
        assert_eq!(
            p.writes,
            vec![vec![0x1b]],
            "idle now, so the popup Esc is safe"
        );
        assert_eq!(fx.poll(&mut inj, 300).writes, vec![vec![b'\r']]);
    }

    #[test]
    fn jobs_are_serialized_with_a_pause() {
        let fx = Fixture::new();
        let mut inj = Injector::default();
        inj.push(Job::keys("Plan", b"\x1bP", Requires::Any));
        inj.push(Job::keys("Hub", b"\x1ba", Requires::Any));
        let p = fx.poll(&mut inj, 0);
        assert_eq!(p.writes, vec![b"\x1bP".to_vec()]);
        assert_eq!(p.wake_in, Some(Duration::from_millis(300)));
        assert!(fx.poll(&mut inj, 299).writes.is_empty());
        assert_eq!(fx.poll(&mut inj, 300).writes, vec![b"\x1ba".to_vec()]);
    }

    #[test]
    fn stop_sends_one_escape_then_a_second_only_if_still_busy() {
        let mut fx = Fixture::new();
        fx.activity = Activity::Busy;
        let mut inj = Injector::default();
        inj.push(Job::stop());
        let p = fx.poll(&mut inj, 0);
        assert_eq!(p.writes, vec![vec![0x1b]]);
        assert_eq!(p.wake_in, Some(Duration::from_millis(650)));
        assert!(fx.poll(&mut inj, 649).writes.is_empty());
        assert_eq!(fx.poll(&mut inj, 650).writes, vec![vec![0x1b]]);

        // The first Esc worked: no second one.
        let mut fx = Fixture::new();
        fx.activity = Activity::Busy;
        let mut inj = Injector::default();
        inj.push(Job::stop());
        fx.poll(&mut inj, 0);
        fx.activity = Activity::Idle;
        let p = fx.poll(&mut inj, 650);
        assert!(p.writes.is_empty() && p.aborted.is_empty());
        assert!(!inj.is_busy());
    }

    #[test]
    fn no_byte_follows_a_lone_escape_within_100ms() {
        let mut fx = Fixture::new();
        fx.activity = Activity::Busy;
        let mut inj = Injector::default();
        inj.push(Job::stop());
        inj.push(Job::keys("Plan", b"\x1bP", Requires::Any));
        let w = drain(&fx, &mut inj);
        assert_eq!(w.len(), 3);
        let gap = |a: usize, b: usize| w[b].0 - w[a].0;
        assert!(gap(0, 1) >= 650 && gap(1, 2) >= 300);
        // Idle slash: Esc is followed by Enter after ≥ 100 ms.
        let fx = Fixture::new();
        let mut inj = Injector::default();
        inj.push(Job::slash("Context", "/context", Requires::Any));
        let w = drain(&fx, &mut inj);
        assert_eq!(w[1].1, vec![0x1b]);
        assert!(w[2].0 - w[1].0 >= 100);
    }

    #[test]
    fn user_input_is_held_from_first_byte_to_150ms_after_the_last() {
        let fx = Fixture::new();
        let mut inj = Injector::default();
        inj.push(Job::slash("Context", "/context", Requires::Any));
        let p = fx.poll(&mut inj, 0);
        assert_eq!(p.writes.len(), 1);
        // Paste written; next step in 150 ms; keep holding through it plus the tail.
        assert_eq!(p.hold_for, Some(Duration::from_millis(300)));
        let p = fx.poll(&mut inj, 150);
        assert_eq!(p.hold_for, Some(Duration::from_millis(300)));
        let p = fx.poll(&mut inj, 300);
        assert_eq!(p.writes, vec![vec![b'\r']]);
        assert_eq!(p.hold_for, Some(HOLD_TAIL));
        assert_eq!(fx.poll(&mut inj, 310).hold_for, None);
    }

    #[test]
    fn no_hold_while_waiting_for_the_users_answer() {
        let mut fx = Fixture::new();
        fx.screen = rail("draft");
        let mut inj = Injector::default();
        inj.push(Job::slash("Compact", "/compact", Requires::Any));
        assert_eq!(fx.poll(&mut inj, 0).hold_for, None);
        inj.answer(true);
        // Ctrl+C goes out now: held through the 650 ms wait plus the tail.
        let p = fx.poll(&mut inj, 10);
        assert_eq!(p.hold_for, Some(Duration::from_millis(800)));
    }

    #[test]
    fn closing_drops_unstarted_work_and_waits_for_a_started_job() {
        let fx = Fixture::new();
        let mut inj = Injector::default();
        inj.push(Job::slash("Context", "/context", Requires::Any));
        inj.push(Job::keys("Plan", b"\x1bP", Requires::Any));
        assert!(inj.drop_unstarted(), "nothing written yet");
        assert!(!inj.is_busy());

        inj.push(Job::slash("Context", "/context", Requires::Any));
        inj.push(Job::keys("Plan", b"\x1bP", Requires::Any));
        fx.poll(&mut inj, 0); // paste written
        assert!(!inj.drop_unstarted(), "mid-job: wait for the boundary");
        // The queued hotkey is gone; the running job finishes, then the injector is idle.
        let w = drain(&fx, &mut inj);
        assert_eq!(w.last().map(|(_, b)| b.clone()), Some(vec![b'\r']));
        assert!(inj.drop_unstarted());
    }

    #[test]
    fn closing_waits_for_the_tail_after_the_last_byte() {
        let fx = Fixture::new();
        let mut inj = Injector::default();
        inj.push(Job::slash("Compact", "/compact", Requires::Idle));
        let w = drain(&fx, &mut inj);
        let Some((at, last)) = w.last().cloned() else {
            panic!("no writes");
        };
        assert_eq!(last, vec![b'\r']);
        let t = |ms: u64| fx.base + Duration::from_millis(ms);
        assert!(!inj.can_close(t(at)), "the Enter was just written");
        assert!(!inj.can_close(t(at + 149)));
        assert!(inj.can_close(t(at + 150)));
    }

    #[test]
    fn titles_cannot_break_out_of_the_bracketed_paste() {
        let job = Job::slash(
            "Rename",
            "/rename a\x1b[201~\r/quit\u{7f}b\u{9b}c\nd\te",
            Requires::Any,
        );
        assert_eq!(
            job.payload,
            Payload::Slash("/rename a[201~ /quitbc d e".to_owned())
        );
        let fx = Fixture::new();
        let mut inj = Injector::default();
        inj.push(job);
        let w = drain(&fx, &mut inj);
        let pasted = &w[0].1;
        assert!(pasted.starts_with(PASTE_START) && pasted.ends_with(PASTE_END));
        let inner = &pasted[PASTE_START.len()..pasted.len() - PASTE_END.len()];
        assert!(inner.iter().all(|b| *b >= 0x20 && *b != 0x7f), "{inner:?}");
    }

    #[test]
    fn sanitize_collapses_whitespace_and_drops_controls() {
        assert_eq!(sanitize_line("  a \n\n b\u{85}c\u{1b}d "), "a b cd");
        assert_eq!(sanitize_line("\u{1b}\u{7f}"), "");
        assert_eq!(sanitize_line("naïve → ok"), "naïve → ok");
    }
}
