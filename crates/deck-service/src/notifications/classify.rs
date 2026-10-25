//! Tells a finished turn from a failed or stopped one by reading the tab's session file. Blocking:
//! runs on a short-lived worker thread, never on the UI thread.

use std::path::Path;
use std::thread;
use std::time::{Duration, SystemTime};

use deck_core::status::{LastTurn, TurnOutcome, last_turn};

use super::inbox::Kind;
use super::tracker::classify;

/// omp appends the assistant message around the time the title flips idle; while the tail does
/// not show this turn's outcome yet (an older reply, no reply, or a still partial last line) the
/// file is read again after this pause …
const RETRY_AFTER: Duration = Duration::from_millis(300);
/// … at most this many times.
const RETRIES: u32 = 4;

/// Tolerated clock/visibility skew between a reply's finish time and the app's busy start.
const SLACK: Duration = Duration::from_secs(2);

/// How the turn that began at `busy_start` ended: `None` when the user stopped it. Without a
/// session file it is a plain finished turn. Otherwise the file is read until it shows this
/// turn's outcome, for at most [`RETRIES`] × [`RETRY_AFTER`]; when it still does not, the turn
/// is reported as a plain finished turn (the title did say omp went idle).
pub fn finished_turn(session: Option<&Path>, busy_start: SystemTime) -> Option<Kind> {
    let Some(path) = session else {
        return Some(Kind::Finished);
    };
    let mut turn = last_turn(path);
    let mut retries = 0;
    while undecided(&turn, busy_start) {
        if retries == RETRIES {
            return Some(Kind::Finished);
        }
        retries += 1;
        thread::sleep(RETRY_AFTER);
        turn = last_turn(path);
    }
    classify(turn.outcome)
}

/// The tail does not show this turn's outcome: no readable assistant reply, or one finished
/// before the turn began.
fn undecided(turn: &LastTurn, busy_start: SystemTime) -> bool {
    matches!(turn.outcome, TurnOutcome::Unknown) || predates(turn, busy_start)
}

/// The deciding assistant message was finished before the turn began. `busy_start` is when the app
/// first saw the tab busy, which lags the real start and may be skewed by clocks, so a reply
/// finishing within [`SLACK`] before it still counts as this turn's.
fn predates(turn: &LastTurn, busy_start: SystemTime) -> bool {
    let earliest = busy_start
        .checked_sub(SLACK)
        .unwrap_or(SystemTime::UNIX_EPOCH);
    turn.assistant_at.is_some_and(|at| at < earliest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn session(lines: &[&str]) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().expect("temp file");
        for l in lines {
            writeln!(f, "{l}").expect("write");
        }
        f
    }

    /// Epoch second of the fixture turn's busy start (a real session's start of 2026-10-25).
    const START_S: u64 = 1_792_896_480;

    fn start() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(START_S)
    }

    /// An assistant entry as omp writes it: `message.timestamp` at request start, `completedAt`
    /// and the outer ISO timestamp at the end (`done_s` seconds after the epoch second).
    fn assistant(reason: &str, extra: &str, done_s: u64) -> String {
        let iso = if done_s >= START_S + 6 {
            "2026-10-25T02:48:06.313Z"
        } else {
            "2026-10-25T02:48:00.000Z"
        };
        format!(
            r#"{{"type":"message","timestamp":"{iso}","message":{{"role":"assistant","timestamp":{start},"completedAt":{done},"stopReason":"{reason}"{extra},"content":[]}}}}"#,
            start = START_S * 1000,
            done = done_s * 1000 + 312,
        )
    }

    #[test]
    fn no_session_file_is_a_plain_finished_turn() {
        assert_eq!(finished_turn(None, SystemTime::now()), Some(Kind::Finished));
    }

    #[test]
    fn a_reply_finished_after_the_turn_began_decides() {
        let done = START_S + 6;
        let err = session(&[&assistant("error", r#","errorMessage":"overloaded""#, done)]);
        assert_eq!(
            finished_turn(Some(err.path()), start()),
            Some(Kind::Error(Some("overloaded".into())))
        );
        let aborted = session(&[&assistant("aborted", "", done)]);
        assert_eq!(finished_turn(Some(aborted.path()), start()), None);
        let ok = session(&[&assistant("stop", "", done)]);
        assert_eq!(
            finished_turn(Some(ok.path()), start()),
            Some(Kind::Finished)
        );
    }

    #[test]
    fn a_reply_finished_within_the_slack_before_the_busy_start_still_decides() {
        let err = session(&[&assistant("error", "", START_S - 1)]);
        assert_eq!(
            finished_turn(Some(err.path()), start()),
            Some(Kind::Error(None))
        );
    }

    #[test]
    fn a_reply_from_an_earlier_turn_falls_back_to_finished() {
        let old_error = session(&[&assistant("error", "", START_S - 3)]);
        assert_eq!(
            finished_turn(Some(old_error.path()), start()),
            Some(Kind::Finished)
        );
    }

    #[test]
    fn a_reply_still_being_appended_is_waited_for() {
        // The title flipped idle while omp was still writing the final line: no Finished verdict
        // from the partial tail, the completed line decides.
        for (reason, extra, want) in [
            ("aborted", "", None),
            (
                "error",
                r#","errorMessage":"overloaded""#,
                Some(Kind::Error(Some("overloaded".into()))),
            ),
        ] {
            let line = assistant(reason, extra, START_S + 6);
            let (head, tail) = line.split_at(line.len() / 2);
            let f = session(&[]);
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(f.path())
                .expect("open");
            file.write_all(head.as_bytes()).expect("write head");
            let tail = tail.to_owned();
            let writer = std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(450));
                writeln!(file, "{tail}").expect("write tail");
            });
            assert_eq!(finished_turn(Some(f.path()), start()), want, "{reason}");
            writer.join().expect("writer");
        }
    }
}
