//! Incremental `.jsonl` tail shared by the summary scanner and the transcript reader, plus
//! the byte-level helpers used to prefilter lines before any JSON parsing.

use std::io::{self, Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use memchr::{memchr, memchr_iter, memmem};

use crate::platform::{self, FileId};

/// Bytes read per [`LineTail::poll`] call.
const MAX_READ: u64 = 8 * 1024 * 1024;
/// A single line longer than this (no newline inside) is dropped instead of buffered.
const MAX_LINE: usize = 48 * 1024 * 1024;

/// What one [`LineTail::poll`] produced.
pub(crate) struct Chunk {
    /// The file was replaced (different file identity) or shrank: the consumer must discard
    /// everything derived from earlier chunks before applying `data`.
    pub reset: bool,
    /// Zero or more complete lines (each terminated by `\n`).
    pub data: Vec<u8>,
    /// More bytes are waiting; poll again.
    pub more: bool,
}

/// Follows an append-only file: file identity + offset + the unterminated last line.
pub(crate) struct LineTail {
    path: PathBuf,
    id: Option<FileId>,
    /// Bytes consumed from the file (including those still held in `pending`).
    offset: u64,
    pending: Vec<u8>,
    /// Inside an over-long line: drop bytes up to the next newline.
    skipping: bool,
}

impl LineTail {
    pub fn new(path: PathBuf) -> Self {
        LineTail {
            path,
            id: None,
            offset: 0,
            pending: Vec::new(),
            skipping: false,
        }
    }

    pub fn poll(&mut self) -> io::Result<Chunk> {
        // Regular files only: a FIFO is never opened, a link never followed (`platform`).
        let mut file = platform::open_regular_file(&self.path)?;
        let md = file.metadata()?;
        let id = platform::file_identity(&file)?;
        let len = md.len();
        let mut reset = false;
        if self.id != Some(id) || len < self.offset {
            reset = self.id.is_some() || self.offset > 0;
            self.id = Some(id);
            self.offset = 0;
            self.pending.clear();
            self.skipping = false;
        }
        if len <= self.offset {
            return Ok(Chunk {
                reset,
                data: Vec::new(),
                more: false,
            });
        }
        let want = (len - self.offset).min(MAX_READ);
        file.seek(SeekFrom::Start(self.offset))?;
        let mut fresh = Vec::new();
        file.take(want).read_to_end(&mut fresh)?;
        self.offset += fresh.len() as u64;
        let more = self.offset < len;
        if self.skipping {
            match memchr(b'\n', &fresh) {
                Some(i) => {
                    fresh.drain(..=i);
                    self.skipping = false;
                }
                None => {
                    return Ok(Chunk {
                        reset,
                        data: Vec::new(),
                        more,
                    });
                }
            }
        }
        self.pending.extend_from_slice(&fresh);
        let data = match memchr::memrchr(b'\n', &self.pending) {
            Some(last) => {
                let rest = self.pending.split_off(last + 1);
                std::mem::replace(&mut self.pending, rest)
            }
            None => Vec::new(),
        };
        if self.pending.len() > MAX_LINE {
            log::debug!(
                "agents: dropping over-long line in {} ({} bytes)",
                self.path.display(),
                self.pending.len()
            );
            self.pending.clear();
            self.skipping = true;
        }
        Ok(Chunk { reset, data, more })
    }
}

/// Complete lines of `data` without their terminators (empty lines skipped).
pub(crate) fn lines(data: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut start = 0;
    memchr_iter(b'\n', data)
        .map(move |end| {
            let line = &data[start..end];
            start = end + 1;
            line.strip_suffix(b"\r").unwrap_or(line)
        })
        .filter(|l| !l.is_empty())
}

/// `{"type":"<kind>"…` → `kind` (borrowed from the line).
pub(crate) fn line_type(line: &[u8]) -> Option<&str> {
    let rest = line.strip_prefix(b"{\"type\":\"")?;
    let end = memchr(b'"', rest.get(..40.min(rest.len()))?)?;
    std::str::from_utf8(&rest[..end]).ok()
}

/// Value of the first `"<key>":"…"` string found in the first `limit` bytes, when it contains
/// no escapes (ids, tool names and roles never do).
pub(crate) fn simple_str<'a>(line: &'a [u8], key: &str, limit: usize) -> Option<&'a str> {
    let needle = format!("\"{key}\":\"");
    let head = &line[..limit.min(line.len())];
    let at = memmem::find(head, needle.as_bytes())? + needle.len();
    let rest = &line[at..];
    let end = memchr(b'"', rest)?;
    if rest[..end].contains(&b'\\') {
        return None;
    }
    std::str::from_utf8(&rest[..end]).ok()
}

/// Entry timestamp: early in `message`-style lines, last field of `custom` lines.
pub(crate) fn line_timestamp(line: &[u8]) -> Option<SystemTime> {
    const KEY: &[u8] = b"\"timestamp\":\"";
    let head = &line[..line.len().min(400)];
    let at = memmem::find(head, KEY).or_else(|| {
        let from = line.len().saturating_sub(96);
        memmem::rfind(&line[from..], KEY).map(|i| i + from)
    })?;
    parse_iso(line.get(at + KEY.len()..)?)
}

/// `2026-10-21T14:19:13.489Z` (prefix of `s`) → `SystemTime`.
pub(crate) fn parse_iso(s: &[u8]) -> Option<SystemTime> {
    fn num(b: &[u8]) -> Option<i64> {
        if b.is_empty() || !b.iter().all(u8::is_ascii_digit) {
            return None;
        }
        std::str::from_utf8(b).ok()?.parse().ok()
    }
    if s.len() < 19 || s[4] != b'-' || s[7] != b'-' || !matches!(s[10], b'T' | b' ') {
        return None;
    }
    let (y, m, d) = (num(&s[0..4])?, num(&s[5..7])?, num(&s[8..10])?);
    let (hh, mm, ss) = (num(&s[11..13])?, num(&s[14..16])?, num(&s[17..19])?);
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) || hh > 23 || mm > 59 || ss > 60 {
        return None;
    }
    let mut nanos = 0u32;
    if s.get(19) == Some(&b'.') {
        let digits: Vec<u8> = s[20..]
            .iter()
            .take_while(|b| b.is_ascii_digit())
            .take(9)
            .copied()
            .collect();
        if !digits.is_empty() {
            nanos = num(&digits)? as u32 * 10u32.pow(9 - digits.len() as u32);
        }
    }
    // days from civil (Howard Hinnant)
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let secs = days * 86_400 + hh * 3600 + mm * 60 + ss;
    let secs = u64::try_from(secs).ok()?;
    Some(SystemTime::UNIX_EPOCH + Duration::new(secs, nanos))
}

/// Epoch milliseconds → `SystemTime`.
pub(crate) fn from_epoch_ms(ms: f64) -> Option<SystemTime> {
    (ms.is_finite() && ms >= 0.0).then(|| SystemTime::UNIX_EPOCH + Duration::from_millis(ms as u64))
}

/// `s` cut to at most `max` bytes on a char boundary; reports whether it was cut.
pub(crate) fn cap_bytes(mut s: String, max: usize) -> (String, bool) {
    if s.len() <= max {
        return (s, false);
    }
    let mut cut = max;
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    s.truncate(cut);
    (s, true)
}

/// First `max` chars of the first non-empty line of `s`, with `…` if cut.
pub(crate) fn one_line(s: &str, max: usize) -> String {
    let line = s
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    match line.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", &line[..i]),
        None => line.to_owned(),
    }
}
