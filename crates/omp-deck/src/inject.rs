//! Per-tab command injection into omp's composer (docs/notes/omp-actions.md §3.7).
//!
//! A pure state machine driven from the frame loop: [`Injector::poll`] takes the current time and
//! the tab's observable state and returns the bytes to write now plus when to look again. It never
//! sleeps and never touches the PTY itself, so the whole protocol is testable with a fake clock.
//!
//! Slash command sequence: (the cursor must be in omp's composer; a draft there → confirm →
//! Ctrl+C, ≥ 650 ms after the last Ctrl+C, then ≥ 650 ms until the composer reads empty) →
//! bracketed paste of the command → the composer shows it and the rows below it hold still →
//! (idle only, and only while omp's autocomplete popup is open) `Esc` → the popup is seen closing →
//! Enter. Direct hotkeys are a single key. Stop is `Esc`, then — only if omp still works after
//! 650 ms — one more `Esc`. Jobs of one tab are strictly serialized with a pause between them.
//!
//! After the paste every step waits for omp's screen to show the previous one took effect, never a
//! fixed delay: omp reads `ESC` + `\r` arriving within its 75 ms escape timeout as Alt+Enter, and a
//! stall anywhere on the way (OmpDeck's PTY thread, the tmux server, ConPTY) delivered the old timed
//! `Esc` … 150 ms … Enter back to back — a newline under a still-open popup, the command unsent.
//! The popup is detected as a change of [`composer::rows_below`] against the empty composer before
//! the paste; without one no `Esc` is sent at all. A step omp never shows in time ([`SHOW_TIMEOUT`],
//! [`ESC_TIMEOUT`]) abandons the job without Enter ([`Poll::stalled`]): the command stays in the
//! composer rather than risk the merge.
//!
//! Text jobs (review comments): the paste bytes come from [`Env::paste`] (the terminal's own mode-aware
//! encoding, evaluated at write time so the pure state machine never guesses the modes); `None` →
//! the job is refused ([`Poll::no_paste`]). Insert (`submit=false`) pastes at omp's cursor and
//! ends there; Send (`submit=true`) follows the slash sequence with the text as the paste.
//! [`Poll::completed`] names the jobs that reached their last step.
//!
//! Keys (Ctrl+C, Esc, Enter, hotkeys) are [`Write::Key`] strokes that the terminal encodes like
//! the user's own key press (`deck_term::Terminal::send_key`), never raw bytes: in
//! win32-input-mode ConPTY keeps a trailing raw `ESC` as the start of a sequence and merges it
//! with the next write, so a raw `ESC` … `\r` reached omp as Alt+Enter and left the command
//! unsent in the composer.
//!
//! Safety rules: a job that started while omp was idle is abandoned (no further bytes) as soon as
//! omp is no longer idle, so a turn that begins mid-sequence is never interrupted by our `Esc` or
//! submitted by our `\r`; the caller drops the user's own keystrokes while a job is in flight
//! ([`Poll::hold_for`]); no byte follows a lone `Esc` within [`AFTER_ESC`].

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use deck_term::ScreenText;
use deck_term::input::{Key, Mods};

use crate::composer::{self, Composer};
use crate::statusfmt::Activity;

/// omp exits on two Ctrl+C within 500 ms; keep clear of that.
pub const CTRL_C_GAP: Duration = Duration::from_millis(650);
/// Time omp needs to clear the composer after Ctrl+C.
const AFTER_CLEAR: Duration = Duration::from_millis(650);
/// Re-check interval while a step waits for omp's screen.
const SCREEN_POLL: Duration = Duration::from_millis(30);
/// omp must show the cleared composer and then the pasted text within this time.
const SHOW_TIMEOUT: Duration = Duration::from_secs(3);
/// The rows below the composer must hold still this long before the popup check: omp opens its
/// autocomplete popup asynchronously, a frame or two after it draws the text.
const SETTLE: Duration = Duration::from_millis(150);
/// Minimum gap between an `Esc` and the next byte.
const AFTER_ESC: Duration = Duration::from_millis(150);
/// The popup must be seen closing within this time after its `Esc`.
const ESC_TIMEOUT: Duration = Duration::from_secs(2);
/// Stop: how long the first `Esc` gets to end the turn before a second one is sent.
const STOP_RETRY: Duration = Duration::from_millis(650);
/// Pause between two jobs of the same tab.
const BETWEEN_JOBS: Duration = Duration::from_millis(300);
/// User input stays dropped this long after the last injected byte.
pub const HOLD_TAIL: Duration = Duration::from_millis(150);

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
    /// One key stroke (hotkeys such as Alt+Shift+P); no composer handling needed.
    Key(Key, Mods),
    /// `Esc`, and a second `Esc` after [`STOP_RETRY`] if the turn still runs.
    Stop,
    /// Review comments and other multi-line text, written with `Terminal::paste_bytes`.
    Text { text: String, submit: bool },
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

/// Multi-line text for a bracketed paste: newlines and tabs stay, every other C0/C1 control
/// (ESC, CR, DEL, NEL …) is dropped so nothing in it can end the paste early or act as a key.
pub fn sanitize_text(s: &str) -> String {
    s.chars()
        .filter(|&c| c == '\n' || c == '\t' || !c.is_control())
        .collect()
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

    /// A multi-line text pasted into omp's composer (`submit=false`: **Insert**, any activity,
    /// keeps a draft, never sends; `submit=true`: **Send**, idle only, the draft is confirmed and
    /// cleared first, then Esc + Enter). The text is sanitized with [`sanitize_text`].
    pub fn text(label: impl Into<String>, text: &str, submit: bool) -> Job {
        Job {
            label: label.into(),
            payload: Payload::Text {
                text: sanitize_text(text),
                submit,
            },
            requires: if submit {
                Requires::Idle
            } else {
                Requires::Any
            },
        }
    }

    pub fn key(label: impl Into<String>, key: Key, mods: Mods, requires: Requires) -> Job {
        Job {
            label: label.into(),
            payload: Payload::Key(key, mods),
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
    /// The exact bytes the terminal's own paste sends for a text under its current modes
    /// (`deck_term::Terminal::paste_bytes`); `None` when omp has not enabled bracketed paste.
    /// Only [`Payload::Text`] jobs use it.
    pub paste: &'a dyn Fn(&str) -> Option<Vec<u8>>,
}

/// One piece of input for the tab's terminal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Write {
    /// Raw bytes (the bracketed paste of a slash command).
    Bytes(Vec<u8>),
    /// A key stroke, to be encoded like the user's own key (`deck_term::Terminal::send_key`).
    Key(Key, Mods),
}

const ESC_KEY: Write = Write::Key(Key::Escape, Mods::NONE);
const ENTER_KEY: Write = Write::Key(Key::Enter, Mods::NONE);
const CTRL_C_KEY: Write = Write::Key(Key::Char('c'), Mods::CTRL);

/// What the caller must do after a poll.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Poll {
    /// Input for the terminal, in order.
    pub writes: Vec<Write>,
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
    /// Jobs abandoned because omp did not show a step in time ([`SHOW_TIMEOUT`]); the command may
    /// sit unsent in the composer.
    pub stalled: Vec<String>,
    /// Jobs that ran to their last step (every byte of them was handed out).
    pub completed: Vec<String>,
    /// Text jobs dropped because omp has not enabled bracketed paste (`Env::paste` was `None`).
    pub no_paste: Vec<String>,
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
    /// The composer must read empty from this instant on (Ctrl+C sent, or nothing to clear).
    Cleared(Instant),
    /// Paste written at `at`; `baseline` = the rows below the empty composer before it.
    /// `settle` = the rows below the composer since it shows the text, and since when.
    Pasted {
        at: Instant,
        baseline: Vec<String>,
        settle: Option<(Vec<String>, Instant)>,
    },
    /// Popup `Esc` sent at `at` while the rows below the composer were `popup`.
    Escaped { at: Instant, popup: Vec<String> },
    /// Stop: the second-`Esc` check is due at this instant.
    Stopping(Instant),
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
                    if let Some(a) = self.active.take() {
                        out.completed.push(a.job.label);
                    }
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
                Step::Stalled => {
                    if let Some(a) = self.active.take() {
                        out.stalled.push(a.job.label);
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
    /// Ended without its final Enter because omp did not show a step in time.
    Stalled,
}

fn write(a: &mut Active, out: &mut Poll, w: Write) {
    a.wrote = true;
    out.writes.push(w);
}

fn step(a: &mut Active, env: &Env<'_>, ctrl_c: &mut Option<Instant>, out: &mut Poll) -> Step {
    let now = env.now;
    let wait_until = |t: Instant| Step::Wait(t.saturating_duration_since(now));
    if a.started_idle && env.activity != Activity::Idle && !matches!(a.stage, Stage::Start) {
        return Step::Aborted;
    }
    match (&a.job.payload, &a.stage) {
        (Payload::Key(key, mods), Stage::Start) => {
            let w = Write::Key(*key, *mods);
            write(a, out, w);
            Step::Done
        }
        (Payload::Key(..), _) => Step::Done,
        (Payload::Stop, Stage::Start) => {
            write(a, out, ESC_KEY);
            a.stage = Stage::Stopping(now + STOP_RETRY);
            wait_until(now + STOP_RETRY)
        }
        (Payload::Stop, Stage::Stopping(until)) => {
            if now < *until {
                return wait_until(*until);
            }
            // Never two Esc within 500 ms; by now the first one was ≥ 650 ms ago.
            if env.activity.is_busy() {
                write(a, out, ESC_KEY);
            }
            Step::Done
        }
        (Payload::Stop, _) => Step::Done,
        (Payload::Slash(_) | Payload::Text { submit: true, .. }, Stage::Start) => {
            if let Payload::Text { text, .. } = &a.job.payload
                && (env.paste)(text).is_none()
            {
                out.no_paste.push(a.job.label.clone());
                return Step::Dropped;
            }
            match composer::inspect(&(env.screen)()) {
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
            }
        }
        (
            Payload::Text {
                submit: false,
                text,
            },
            Stage::Start,
        ) => {
            // Insert: pasted at omp's cursor; a draft stays, nothing is cleared, nothing submitted.
            let Some(bytes) = (env.paste)(text) else {
                out.no_paste.push(a.job.label.clone());
                return Step::Dropped;
            };
            if matches!(composer::inspect(&(env.screen)()), Composer::Missing) {
                out.dialog_open.push(a.job.label.clone());
                return Step::Dropped;
            }
            write(a, out, Write::Bytes(bytes));
            Step::Done
        }
        (Payload::Text { submit: false, .. }, _) => Step::Done,
        (Payload::Slash(_) | Payload::Text { .. }, Stage::AwaitConfirm) => Step::Blocked,
        (Payload::Slash(_) | Payload::Text { .. }, Stage::ClearPending) => {
            let last = [*ctrl_c, env.last_ctrl_c].into_iter().flatten().max();
            match last {
                Some(t) if now < t + CTRL_C_GAP => wait_until(t + CTRL_C_GAP),
                _ => {
                    write(a, out, CTRL_C_KEY);
                    *ctrl_c = Some(now);
                    a.stage = Stage::Cleared(now + AFTER_CLEAR);
                    wait_until(now + AFTER_CLEAR)
                }
            }
        }
        (Payload::Slash(_) | Payload::Text { .. }, Stage::Cleared(until)) => {
            let until = *until;
            if now < until {
                return wait_until(until);
            }
            // Pasting onto a draft omp has not cleared yet would submit the two of them as one.
            let screen = (env.screen)();
            if composer::inspect(&screen) != Composer::Empty {
                return if now >= until + SHOW_TIMEOUT {
                    Step::Stalled
                } else {
                    Step::Wait(SCREEN_POLL)
                };
            }
            let bytes = match &a.job.payload {
                Payload::Slash(text) => [PASTE_START, text.as_bytes(), PASTE_END].concat(),
                Payload::Text { text, .. } => match (env.paste)(text) {
                    Some(bytes) => bytes,
                    // The terminal's modes changed since the start: nothing was sent for this paste.
                    None => return Step::Aborted,
                },
                Payload::Key(..) | Payload::Stop => return Step::Done,
            };
            let baseline = composer::rows_below(&screen).unwrap_or_default();
            write(a, out, Write::Bytes(bytes));
            a.stage = Stage::Pasted {
                at: now,
                baseline,
                settle: None,
            };
            Step::Wait(SCREEN_POLL)
        }
        (Payload::Slash(_) | Payload::Text { .. }, Stage::Pasted { .. }) => {
            after_paste(a, env, out)
        }
        (Payload::Slash(_) | Payload::Text { .. }, Stage::Escaped { .. }) => {
            after_escape(a, env, out)
        }
        (Payload::Slash(_) | Payload::Text { .. }, Stage::Stopping(_)) => Step::Done,
    }
}

/// The composer shows the job's paste: the command itself (omp may draw an argument hint after
/// it, and wraps long lines), or any text for a review (omp collapses long pastes into a marker).
fn shows_paste(payload: &Payload, screen: &ScreenText) -> bool {
    let Composer::Draft(draft) = composer::inspect(screen) else {
        return false;
    };
    match payload {
        Payload::Slash(text) => {
            let squash = |s: &str| -> String {
                s.chars()
                    .filter(|c| !c.is_whitespace() && *c != '↵')
                    .collect()
            };
            squash(&draft).starts_with(&squash(text))
        }
        _ => true,
    }
}

/// Paste written: once omp shows it, Enter right away (no popup, or omp busy: an `Esc` would
/// interrupt the turn), else `Esc` to close the popup first.
fn after_paste(a: &mut Active, env: &Env<'_>, out: &mut Poll) -> Step {
    let now = env.now;
    let Stage::Pasted {
        at,
        baseline,
        settle,
    } = &mut a.stage
    else {
        return Step::Done;
    };
    let screen = (env.screen)();
    if !shows_paste(&a.job.payload, &screen) {
        return if now >= *at + SHOW_TIMEOUT {
            Step::Stalled
        } else {
            Step::Wait(SCREEN_POLL)
        };
    }
    let popup = if env.activity == Activity::Idle {
        let below = composer::rows_below(&screen).unwrap_or_default();
        match settle {
            Some((rows, since)) if *rows == below => {
                let due = *since + SETTLE;
                if now < due {
                    return Step::Wait(SCREEN_POLL.min(due - now));
                }
            }
            _ => {
                *settle = Some((below, now));
                return Step::Wait(SCREEN_POLL);
            }
        }
        (below != *baseline).then_some(below)
    } else {
        None
    };
    match popup {
        Some(popup) => {
            write(a, out, ESC_KEY);
            a.stage = Stage::Escaped { at: now, popup };
            Step::Wait(AFTER_ESC)
        }
        None => {
            write(a, out, ENTER_KEY);
            Step::Done
        }
    }
}

/// Popup `Esc` written: Enter once the rows below the composer changed, i.e. omp read the `Esc` on
/// its own and closed the popup. Without that proof an Enter could still reach omp together with
/// the `Esc` (Alt+Enter), so the job gives up after [`ESC_TIMEOUT`].
fn after_escape(a: &mut Active, env: &Env<'_>, out: &mut Poll) -> Step {
    let now = env.now;
    let Stage::Escaped { at, popup } = &a.stage else {
        return Step::Done;
    };
    let due = *at + AFTER_ESC;
    if now < due {
        return Step::Wait(due - now);
    }
    if composer::rows_below(&(env.screen)()).is_some_and(|rows| rows != *popup) {
        write(a, out, ENTER_KEY);
        Step::Done
    } else if now >= *at + ESC_TIMEOUT {
        Step::Stalled
    } else {
        Step::Wait(SCREEN_POLL)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALT_SHIFT: Mods = Mods::new(true, true, false);
    const STATUS: &str = " π · ◒ Fake One · 🗑 w3f/proj · ◫ 7.4%/100K ⟲";
    /// omp's autocomplete popup under the composer.
    const POPUP: [&str; 2] = ["> a/b     Model B", "  a/bc    Model BC"];

    fn paste(text: &str) -> Write {
        Write::Bytes([PASTE_START, text.as_bytes(), PASTE_END].concat())
    }

    fn plan_job(label: &str) -> Job {
        Job::key(label, Key::Char('p'), ALT_SHIFT, Requires::Any)
    }

    /// Rail composer (omp 18.12.5) holding `draft`, cursor at its end, `below` under it.
    fn rail_screen(draft: &str, below: &[&str]) -> ScreenText {
        let mut lines: Vec<String> = vec!["".into(), "  v18.12.5".into()];
        for row in draft.split('\n') {
            lines.push(format!("▎ {row}").trim_end().to_owned());
        }
        let cursor_row = lines.len() - 1;
        let cursor_col = lines[cursor_row].chars().count().max(2);
        lines.extend(below.iter().map(|r| (*r).to_owned()));
        ScreenText {
            lines,
            cursor_row,
            cursor_col,
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

    /// A scripted omp: what it draws follows the injector's writes (unless `react` is off, as when
    /// the bytes are stuck on the way).
    struct Fixture {
        base: Instant,
        activity: Activity,
        paste_ok: bool,
        last_ctrl_c: Option<Instant>,
        /// Shown instead of the simulated composer.
        fixed: Option<ScreenText>,
        draft: String,
        popup: bool,
        /// A pasted slash command opens the autocomplete popup.
        paste_opens_popup: bool,
        react: bool,
        /// Bytes from the first `Esc` on are stuck on their way to omp.
        stuck_from_esc: bool,
        /// An Enter reached omp while its popup was open (it would apply a completion).
        enter_into_popup: bool,
    }

    impl Fixture {
        fn new() -> Fixture {
            Fixture {
                base: Instant::now(),
                activity: Activity::Idle,
                paste_ok: true,
                last_ctrl_c: None,
                fixed: None,
                draft: String::new(),
                popup: false,
                paste_opens_popup: true,
                react: true,
                stuck_from_esc: false,
                enter_into_popup: false,
            }
        }

        fn screen(&self) -> ScreenText {
            if let Some(s) = &self.fixed {
                return s.clone();
            }
            let below: &[&str] = if self.popup { &POPUP } else { &["", STATUS] };
            rail_screen(&self.draft, below)
        }

        fn poll(&mut self, inj: &mut Injector, ms: u64) -> Poll {
            let screen = self.screen();
            let f = move || screen.clone();
            let paste_ok = self.paste_ok;
            let p = inj.poll(&Env {
                now: self.base + Duration::from_millis(ms),
                activity: self.activity,
                last_ctrl_c: self.last_ctrl_c,
                screen: &f,
                paste: &|t| paste_ok.then(|| [PASTE_START, t.as_bytes(), PASTE_END].concat()),
            });
            for w in &p.writes {
                self.apply(w);
            }
            p
        }

        fn apply(&mut self, w: &Write) {
            if self.stuck_from_esc && *w == ESC_KEY {
                self.react = false;
            }
            if !self.react {
                return;
            }
            match w {
                Write::Bytes(b) => {
                    let inner = &b[PASTE_START.len()..b.len() - PASTE_END.len()];
                    self.draft.push_str(&String::from_utf8_lossy(inner));
                    self.popup = self.paste_opens_popup && self.draft.starts_with('/');
                }
                Write::Key(Key::Escape, _) => self.popup = false,
                Write::Key(Key::Enter, _) => {
                    self.enter_into_popup |= self.popup;
                    self.draft.clear();
                    self.popup = false;
                }
                // Like omp: Ctrl+C empties the composer but leaves a stale popup on screen.
                Write::Key(Key::Char('c'), m) if m.ctrl => self.draft.clear(),
                Write::Key(..) => {}
            }
        }
    }

    fn drain(fx: &mut Fixture, inj: &mut Injector) -> Vec<(u64, Write)> {
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

    fn keys(w: &[(u64, Write)]) -> Vec<Write> {
        w.iter().map(|(_, k)| k.clone()).collect()
    }

    fn at(w: &[(u64, Write)], key: &Write) -> u64 {
        w.iter()
            .find(|(_, k)| k == key)
            .map(|(t, _)| *t)
            .unwrap_or_else(|| panic!("{key:?} never written: {w:?}"))
    }

    #[test]
    fn popup_is_closed_with_escape_before_the_enter() {
        let mut fx = Fixture::new();
        let mut inj = Injector::default();
        inj.push(Job::slash("Switch model", "/switch a/b", Requires::Any));
        let w = drain(&mut fx, &mut inj);
        assert_eq!(keys(&w), vec![paste("/switch a/b"), ESC_KEY, ENTER_KEY]);
        assert!(!fx.enter_into_popup && fx.draft.is_empty());
        // The popup opens a frame after the text: it gets time to show before the decision.
        assert!(at(&w, &ESC_KEY) >= SETTLE.as_millis() as u64);
        assert!(at(&w, &ENTER_KEY) - at(&w, &ESC_KEY) >= AFTER_ESC.as_millis() as u64);
    }

    #[test]
    fn without_a_popup_no_escape_is_sent() {
        let mut fx = Fixture::new();
        fx.paste_opens_popup = false;
        let mut inj = Injector::default();
        inj.push(Job::slash("Rename", "/rename new title", Requires::Any));
        let w = drain(&mut fx, &mut inj);
        assert_eq!(keys(&w), vec![paste("/rename new title"), ENTER_KEY]);
        assert!(fx.draft.is_empty());
    }

    /// The bug: the Esc and the Enter reached omp back to back (Alt+Enter, a newline under a still
    /// open popup, the command unsent). An Enter only follows once omp is seen reading the Esc.
    #[test]
    fn enter_never_follows_an_escape_omp_has_not_shown() {
        let mut fx = Fixture::new();
        fx.stuck_from_esc = true;
        let mut inj = Injector::default();
        inj.push(Job::slash("Switch model", "/switch a/b", Requires::Any));
        let mut writes = Vec::new();
        let mut stalled = Vec::new();
        let mut t = 0;
        while inj.is_busy() && t < 10_000 {
            let p = fx.poll(&mut inj, t);
            writes.extend(p.writes);
            stalled.extend(p.stalled);
            t += 10;
        }
        assert_eq!(writes, vec![paste("/switch a/b"), ESC_KEY]);
        assert_eq!(stalled, vec!["Switch model".to_owned()]);
        // The Esc arrives late after all: the Enter would have merged with it, none was sent.
        assert!(fx.popup);
    }

    #[test]
    fn a_late_popup_close_still_gets_its_enter() {
        let mut fx = Fixture::new();
        fx.stuck_from_esc = true;
        let mut inj = Injector::default();
        inj.push(Job::slash("Switch model", "/switch a/b", Requires::Any));
        let mut writes = Vec::new();
        let mut esc_at = None;
        for t in (0..5_000).step_by(10) {
            if esc_at.is_some_and(|e| t >= e + 1_200) {
                fx.popup = false; // omp finally read the Esc
                fx.react = true;
            }
            let p = fx.poll(&mut inj, t);
            if p.writes.contains(&ESC_KEY) {
                esc_at = Some(t);
            }
            writes.extend(p.writes.into_iter().map(|w| (t, w)));
        }
        let esc = esc_at.expect("an Esc");
        assert_eq!(
            keys(&writes),
            vec![paste("/switch a/b"), ESC_KEY, ENTER_KEY]
        );
        assert!(at(&writes, &ENTER_KEY) >= esc + 1_200);
    }

    #[test]
    fn a_paste_omp_never_shows_stalls_without_escape_or_enter() {
        let mut fx = Fixture::new();
        fx.react = false;
        let mut inj = Injector::default();
        inj.push(Job::slash("Compact", "/compact", Requires::Idle));
        let mut stalled_at = None;
        let mut writes = Vec::new();
        for t in (0..5_000).step_by(10) {
            let p = fx.poll(&mut inj, t);
            writes.extend(p.writes);
            if !p.stalled.is_empty() && stalled_at.is_none() {
                stalled_at = Some(t);
            }
        }
        assert_eq!(writes, vec![paste("/compact")]);
        assert_eq!(stalled_at, Some(SHOW_TIMEOUT.as_millis() as u64));
    }

    #[test]
    fn a_draft_left_by_the_clear_is_never_pasted_onto() {
        let mut fx = Fixture::new();
        fx.draft = "half typed".into();
        let mut inj = Injector::default();
        inj.push(Job::slash("Compact", "/compact", Requires::Idle));
        fx.poll(&mut inj, 0);
        inj.answer(true);
        fx.react = false; // the Ctrl+C does not clear it
        let mut writes = Vec::new();
        let mut stalled = Vec::new();
        for t in (0..6_000).step_by(10) {
            let p = fx.poll(&mut inj, t);
            writes.extend(p.writes);
            stalled.extend(p.stalled);
        }
        assert_eq!(writes, vec![CTRL_C_KEY]);
        assert_eq!(stalled, vec!["Compact".to_owned()]);
    }

    #[test]
    fn busy_slash_skips_escape_even_with_a_popup() {
        let mut fx = Fixture::new();
        fx.activity = Activity::Busy;
        let mut inj = Injector::default();
        inj.push(Job::slash("Switch", "/effort low", Requires::Any));
        let w = drain(&mut fx, &mut inj);
        assert_eq!(keys(&w), vec![paste("/effort low"), ENTER_KEY]);
    }

    #[test]
    fn attention_and_unknown_slash_skip_escape_too() {
        for activity in [Activity::Attention, Activity::Unknown] {
            let mut fx = Fixture::new();
            fx.activity = activity;
            let mut inj = Injector::default();
            inj.push(Job::slash("Context", "/context", Requires::Any));
            let w = drain(&mut fx, &mut inj);
            assert_eq!(keys(&w), vec![paste("/context"), ENTER_KEY], "{activity:?}");
        }
    }

    #[test]
    fn draft_asks_then_clears_with_ctrl_c_and_waits() {
        let mut fx = Fixture::new();
        fx.draft = "half typed".into();
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
        assert_eq!(p.writes, vec![CTRL_C_KEY]);
        assert_eq!(p.wake_in, Some(Duration::from_millis(650)));
        assert!(fx.poll(&mut inj, 5_649).writes.is_empty());
        let p = fx.poll(&mut inj, 5_650);
        assert_eq!(p.writes, vec![paste("/compact soft")]);
    }

    /// A retry: Ctrl+C left the earlier popup on screen and the same command opens it again.
    #[test]
    fn retry_over_a_stale_popup_submits() {
        let mut fx = Fixture::new();
        fx.draft = "/switch a/b".into();
        fx.popup = true;
        let mut inj = Injector::default();
        inj.push(Job::slash("Switch model", "/switch a/b", Requires::Any));
        fx.poll(&mut inj, 0);
        inj.answer(true);
        let mut writes = Vec::new();
        let mut stalled = Vec::new();
        for t in (0..5_000).step_by(10) {
            let p = fx.poll(&mut inj, t);
            writes.extend(p.writes);
            stalled.extend(p.stalled);
        }
        assert!(stalled.is_empty());
        assert_eq!(writes.first(), Some(&CTRL_C_KEY));
        assert_eq!(writes.last(), Some(&ENTER_KEY));
        assert!(fx.draft.is_empty());
    }

    #[test]
    fn multiline_draft_with_cursor_on_empty_last_line_asks() {
        let mut fx = Fixture::new();
        fx.fixed = Some(ScreenText {
            lines: vec!["".into(), "▎ one".into(), "▎ two".into(), "▎".into()],
            cursor_row: 3,
            cursor_col: 2,
        });
        let mut inj = Injector::default();
        inj.push(Job::slash("Handoff", "/handoff", Requires::Idle));
        let p = fx.poll(&mut inj, 0);
        assert_eq!(p.confirm.map(|c| c.draft), Some("one ↵ two".to_owned()));
    }

    #[test]
    fn declined_draft_drops_the_job() {
        let mut fx = Fixture::new();
        fx.draft = "hello".into();
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
        fx.fixed = Some(overlay());
        let mut inj = Injector::default();
        inj.push(Job::slash("Compact", "/compact", Requires::Idle));
        inj.push(plan_job("Plan mode"));
        let p = fx.poll(&mut inj, 0);
        assert_eq!(p.dialog_open, vec!["Compact".to_owned()]);
        // The refusal costs no pause: the hotkey runs in the same poll.
        assert_eq!(p.writes, vec![Write::Key(Key::Char('p'), ALT_SHIFT)]);
    }

    #[test]
    fn ctrl_c_respects_users_recent_ctrl_c() {
        let mut fx = Fixture::new();
        fx.draft = "draft".into();
        fx.last_ctrl_c = Some(fx.base + Duration::from_millis(100));
        let mut inj = Injector::default();
        inj.push(Job::slash("Compact", "/compact", Requires::Any));
        fx.poll(&mut inj, 200);
        inj.answer(true);
        let p = fx.poll(&mut inj, 200);
        assert!(p.writes.is_empty());
        assert_eq!(p.wake_in, Some(Duration::from_millis(550)));
        let p = fx.poll(&mut inj, 750);
        assert_eq!(p.writes, vec![CTRL_C_KEY]);
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
        assert_eq!(fx.poll(&mut inj, 1_000).writes, vec![ESC_KEY]);
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
        assert!(drain(&mut fx, &mut inj).is_empty());
    }

    #[test]
    fn leaving_idle_between_escape_and_enter_aborts() {
        let mut fx = Fixture::new();
        let mut inj = Injector::default();
        inj.push(Job::slash("Context", "/context", Requires::Any));
        let mut t = 0;
        while !fx.poll(&mut inj, t).writes.contains(&ESC_KEY) {
            t += 10;
            assert!(t < 5_000, "no Esc");
        }
        fx.activity = Activity::Attention;
        let p = fx.poll(&mut inj, t + 300);
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
        let mut writes = Vec::new();
        for t in (10..3_000).step_by(10) {
            let p = fx.poll(&mut inj, t);
            assert!(p.aborted.is_empty());
            writes.extend(p.writes);
        }
        // Idle now, so the popup Esc is safe.
        assert_eq!(writes, vec![ESC_KEY, ENTER_KEY]);
    }

    #[test]
    fn jobs_are_serialized_with_a_pause() {
        let mut fx = Fixture::new();
        let mut inj = Injector::default();
        inj.push(plan_job("Plan"));
        inj.push(Job::key("Hub", Key::Char('a'), Mods::ALT, Requires::Any));
        let p = fx.poll(&mut inj, 0);
        assert_eq!(p.writes, vec![Write::Key(Key::Char('p'), ALT_SHIFT)]);
        assert_eq!(p.wake_in, Some(Duration::from_millis(300)));
        assert!(fx.poll(&mut inj, 299).writes.is_empty());
        assert_eq!(
            fx.poll(&mut inj, 300).writes,
            vec![Write::Key(Key::Char('a'), Mods::ALT)]
        );
    }

    #[test]
    fn stop_sends_one_escape_then_a_second_only_if_still_busy() {
        let mut fx = Fixture::new();
        fx.activity = Activity::Busy;
        let mut inj = Injector::default();
        inj.push(Job::stop());
        let p = fx.poll(&mut inj, 0);
        assert_eq!(p.writes, vec![ESC_KEY]);
        assert_eq!(p.wake_in, Some(Duration::from_millis(650)));
        assert!(fx.poll(&mut inj, 649).writes.is_empty());
        assert_eq!(fx.poll(&mut inj, 650).writes, vec![ESC_KEY]);

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
        inj.push(plan_job("Plan"));
        let w = drain(&mut fx, &mut inj);
        assert_eq!(w.len(), 3);
        let gap = |a: usize, b: usize| w[b].0 - w[a].0;
        assert!(gap(0, 1) >= 650 && gap(1, 2) >= 300);
    }

    #[test]
    fn user_input_is_held_while_a_started_job_waits() {
        let mut fx = Fixture::new();
        let mut inj = Injector::default();
        inj.push(Job::slash("Context", "/context", Requires::Any));
        let mut t = 0;
        let mut hold_until = 0;
        while inj.is_busy() {
            let p = fx.poll(&mut inj, t);
            let next = t + p.wake_in.map_or(0, |d| d.as_millis() as u64);
            if inj.is_busy() {
                // The hold always reaches past the next step.
                let h = p.hold_for.expect("hold while the job runs");
                assert!(t + h.as_millis() as u64 > next, "t={t}");
            }
            if let Some(h) = p.hold_for {
                hold_until = t + h.as_millis() as u64;
            }
            t = next.max(t + 10);
            assert!(t < 5_000);
        }
        let last = t - 10;
        assert!(hold_until >= last + HOLD_TAIL.as_millis() as u64 - 10);
        assert_eq!(fx.poll(&mut inj, t + 500).hold_for, None);
    }

    #[test]
    fn no_hold_while_waiting_for_the_users_answer() {
        let mut fx = Fixture::new();
        fx.draft = "draft".into();
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
        let mut fx = Fixture::new();
        let mut inj = Injector::default();
        inj.push(Job::slash("Context", "/context", Requires::Any));
        inj.push(plan_job("Plan"));
        assert!(inj.drop_unstarted(), "nothing written yet");
        assert!(!inj.is_busy());

        inj.push(Job::slash("Context", "/context", Requires::Any));
        inj.push(plan_job("Plan"));
        fx.poll(&mut inj, 0); // paste written
        assert!(!inj.drop_unstarted(), "mid-job: wait for the boundary");
        // The queued hotkey is gone; the running job finishes, then the injector is idle.
        let w = drain(&mut fx, &mut inj);
        assert_eq!(w.last().map(|(_, b)| b.clone()), Some(ENTER_KEY));
        assert!(inj.drop_unstarted());
    }

    #[test]
    fn closing_waits_for_the_tail_after_the_last_byte() {
        let mut fx = Fixture::new();
        let mut inj = Injector::default();
        inj.push(Job::slash("Compact", "/compact", Requires::Idle));
        let w = drain(&mut fx, &mut inj);
        let Some((at, last)) = w.last().cloned() else {
            panic!("no writes");
        };
        assert_eq!(last, ENTER_KEY);
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
        let mut fx = Fixture::new();
        let mut inj = Injector::default();
        inj.push(job);
        let w = drain(&mut fx, &mut inj);
        let Write::Bytes(pasted) = &w[0].1 else {
            panic!("the paste comes first: {w:?}");
        };
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

    // ---- Payload::Text (review comments)

    const REVIEW: &str = "Review:\n1. a.rs:3\n> x\nfix\tthis";

    #[test]
    fn text_is_sanitized_keeping_newlines_and_tabs() {
        let job = Job::text("Send review", "a\nb\tc\u{1b}[201~\r\n\u{7f}d\u{85}e", true);
        assert_eq!(
            job.payload,
            Payload::Text {
                text: "a\nb\tc[201~\nde".to_owned(),
                submit: true
            }
        );
        assert_eq!(job.requires, Requires::Idle);
        assert_eq!(Job::text("Insert", "x", false).requires, Requires::Any);
    }

    #[test]
    fn insert_pastes_once_without_clearing_confirming_or_submitting() {
        let mut fx = Fixture::new();
        fx.draft = "half-written draft".into();
        fx.activity = Activity::Busy;
        let mut inj = Injector::default();
        inj.push(Job::text("Insert review", REVIEW, false));
        let p = fx.poll(&mut inj, 0);
        assert_eq!(p.confirm, None);
        assert_eq!(p.writes, vec![paste(REVIEW)]);
        assert_eq!(p.completed, vec!["Insert review".to_owned()]);
        assert!(!inj.is_busy());
    }

    #[test]
    fn insert_is_refused_without_a_composer_and_costs_no_bytes() {
        let mut fx = Fixture::new();
        fx.fixed = Some(overlay());
        let mut inj = Injector::default();
        inj.push(Job::text("Insert review", REVIEW, false));
        let p = fx.poll(&mut inj, 0);
        assert_eq!(p.dialog_open, vec!["Insert review".to_owned()]);
        assert!(p.writes.is_empty() && p.completed.is_empty());
    }

    #[test]
    fn text_without_bracketed_paste_is_refused_with_a_note() {
        for submit in [false, true] {
            let mut fx = Fixture::new();
            fx.paste_ok = false;
            let mut inj = Injector::default();
            inj.push(Job::text("Review", REVIEW, submit));
            let p = fx.poll(&mut inj, 0);
            assert_eq!(p.no_paste, vec!["Review".to_owned()], "submit={submit}");
            assert!(p.writes.is_empty() && !inj.is_busy());
        }
    }

    #[test]
    fn send_pastes_then_enters_and_reports_completion() {
        let mut fx = Fixture::new();
        let mut inj = Injector::default();
        inj.push(Job::text("Send review", REVIEW, true));
        let mut writes = Vec::new();
        let mut done = Vec::new();
        for t in (0..3_000).step_by(10) {
            let p = fx.poll(&mut inj, t);
            writes.extend(p.writes);
            done.extend(p.completed);
        }
        assert_eq!(writes, vec![paste(REVIEW), ENTER_KEY]);
        assert_eq!(done, vec!["Send review".to_owned()]);
        assert!(fx.draft.is_empty());
    }

    #[test]
    fn send_needs_idle() {
        for activity in [Activity::Busy, Activity::Attention, Activity::Unknown] {
            let mut fx = Fixture::new();
            fx.activity = activity;
            let mut inj = Injector::default();
            inj.push(Job::text("Send review", REVIEW, true));
            let p = fx.poll(&mut inj, 0);
            assert_eq!(
                p.refused,
                vec![("Send review".to_owned(), Requires::Idle)],
                "{activity:?}"
            );
            assert!(p.writes.is_empty());
        }
    }

    #[test]
    fn send_confirms_a_draft_then_clears_it_with_ctrl_c_before_pasting() {
        let mut fx = Fixture::new();
        fx.draft = "unsent words".into();
        let mut inj = Injector::default();
        inj.push(Job::text("Send review", REVIEW, true));
        let p = fx.poll(&mut inj, 0);
        assert_eq!(p.confirm.map(|c| c.draft), Some("unsent words".to_owned()));
        assert!(p.writes.is_empty());
        inj.answer(true);
        let mut writes = Vec::new();
        for t in (0..3_000).step_by(10) {
            writes.extend(fx.poll(&mut inj, t).writes);
        }
        assert_eq!(writes, vec![CTRL_C_KEY, paste(REVIEW), ENTER_KEY]);
    }

    #[test]
    fn declining_the_draft_confirmation_drops_the_send() {
        let mut fx = Fixture::new();
        fx.draft = "unsent words".into();
        let mut inj = Injector::default();
        inj.push(Job::text("Send review", REVIEW, true));
        fx.poll(&mut inj, 0);
        inj.answer(false);
        assert!(!inj.is_busy());
        let p = fx.poll(&mut inj, 100);
        assert!(p.writes.is_empty() && p.completed.is_empty());
    }

    #[test]
    fn send_is_abandoned_when_omp_starts_working_mid_sequence() {
        let mut fx = Fixture::new();
        let mut inj = Injector::default();
        inj.push(Job::text("Send review", REVIEW, true));
        let p = fx.poll(&mut inj, 0);
        assert_eq!(p.writes, vec![paste(REVIEW)]);
        fx.activity = Activity::Busy;
        let p = fx.poll(&mut inj, 200);
        assert_eq!(p.aborted, vec!["Send review".to_owned()]);
        assert!(p.writes.is_empty() && p.completed.is_empty());
    }
}
