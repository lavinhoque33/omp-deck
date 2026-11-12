//! Incremental turn attribution. Strings are decoded and capped before serde sees them, so a
//! single huge text/image/tool-argument line cannot make a waiter allocate the whole transcript.

use std::borrow::Cow;
use std::cell::RefCell;
use std::fmt;
use std::fs::File;
use std::io::{self, BufReader, Read, Seek, SeekFrom};
use std::path::PathBuf;

use serde::Deserializer;
use serde::de::{DeserializeSeed, IgnoredAny, MapAccess, SeqAccess, Visitor};

use super::{Part, TurnOutcome, turn_outcome};

const REPLY_CHARS: usize = 32 * 1024;
const READ_BYTES: usize = 64 * 1024;

/// The prompt's own turn, not a later human/agent turn in the same session file.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TurnScan {
    pub prompt_seen: bool,
    pub end: Option<TurnOutcome>,
    pub reply: Option<String>,
    pub reply_more: usize,
    pub later_prompt: bool,
}

/// Reads complete appended JSONL entries with bounded memory; incomplete lines remain on disk
/// until their newline arrives. A shrink invalidates the baseline rather than silently restarting.
pub struct TurnWatch {
    path: PathBuf,
    scanned_to: u64,
    line_start: u64,
    prompt: Option<String>,
    exact: bool,
    cap: usize,
    scan: TurnScan,
}

impl TurnWatch {
    pub fn new(path: PathBuf, from: u64, prompt: Option<String>) -> TurnWatch {
        let cap = prompt
            .as_deref()
            .map(str::trim)
            .map_or(REPLY_CHARS, |p| REPLY_CHARS.max(p.chars().count()));
        Self {
            path,
            scanned_to: from,
            line_start: from,
            prompt,
            exact: false,
            cap,
            scan: TurnScan::default(),
        }
    }

    /// Whether the current attribution exactly matches the requested prompt.
    /// Callers must not finalize a fallback-attributed turn while the session is still busy.
    pub fn matched_exactly(&self) -> bool {
        self.exact
    }

    pub fn poll(&mut self) -> io::Result<TurnScan> {
        let mut input = File::open(&self.path)?;
        let length = input.metadata()?.len();
        if length < self.scanned_to {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "session file rewritten",
            ));
        }
        if length == self.scanned_to {
            return Ok(self.scan.clone());
        }
        input.seek(SeekFrom::Start(self.scanned_to))?;
        // Separate cursors: discover complete line spans without ever buffering a whole line.
        // Only a completed span is parsed; an incomplete span is not reparsed on every poll.
        let mut entries = BufReader::with_capacity(READ_BYTES, File::open(&self.path)?);
        let mut buffer = [0u8; READ_BYTES];
        while self.scanned_to < length {
            let want = (length - self.scanned_to).min(READ_BYTES as u64) as usize;
            let count = input.read(&mut buffer[..want])?;
            if count == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "session file rewritten",
                ));
            }
            let base = self.scanned_to;
            for at in memchr::memchr_iter(b'\n', &buffer[..count]) {
                let end = base + at as u64 + 1;
                entries.seek(SeekFrom::Start(self.line_start))?;
                let text = RefCell::new(Text::default());
                let capped = CappedJson {
                    input: (&mut entries).take(end - self.line_start),
                    token: Vec::new(),
                    at: 0,
                    text: &text,
                    cap: self.cap,
                };
                let mut decoder = serde_json::Deserializer::from_reader(capped);
                match (EntrySeed {
                    text: &text,
                    cap: self.cap,
                })
                .deserialize(&mut decoder)
                {
                    Ok(entry) if decoder.end().is_ok() => self.apply(entry),
                    Ok(_) => {
                        log::debug!("turn watch: skipping malformed line at {}", self.line_start)
                    }
                    Err(error) => log::debug!(
                        "turn watch: skipping malformed line at {}: {error}",
                        self.line_start
                    ),
                }
                self.line_start = end;
            }
            self.scanned_to += count as u64;
        }
        Ok(self.scan.clone())
    }

    fn apply(&mut self, entry: Entry) {
        if entry.kind.as_deref() != Some("message") {
            return;
        }
        let Some(message) = entry.message else { return };
        match message.role.as_deref() {
            Some("user") => {
                let matches = self.prompt.as_deref().is_none_or(|prompt| {
                    let wanted = prompt.trim();
                    message.content.trimmed_chars == wanted.chars().count()
                        && message.content.after_leading.trim_end() == wanted
                });
                if !self.scan.prompt_seen || (!self.exact && matches) {
                    self.scan = TurnScan {
                        prompt_seen: true,
                        ..TurnScan::default()
                    };
                    self.exact = matches;
                } else {
                    self.scan.later_prompt = true;
                }
            }
            Some("assistant") if self.scan.prompt_seen && !self.scan.later_prompt => {
                if message.content.chars > 0 {
                    self.scan.reply_more = message.content.chars.saturating_sub(REPLY_CHARS);
                    let mut reply = message.content.prefix;
                    if let Some((cut, _)) = reply.char_indices().nth(REPLY_CHARS) {
                        reply.truncate(cut);
                    }
                    self.scan.reply = Some(reply);
                }
                if message.stop.as_deref() != Some("toolUse") {
                    self.scan.end = Some(turn_outcome(
                        message.stop.as_deref(),
                        message.error.as_deref(),
                    ));
                }
            }
            _ => {}
        }
    }
}

/// Both the raw prefix (reply) and the prefix after leading whitespace (exact trimmed prompt
/// matching) are retained. Counts cover the entire string, even beyond the retained prefixes.
#[derive(Default)]
struct Text {
    prefix: String,
    prefix_chars: usize,
    chars: usize,
    leading: usize,
    after_leading: String,
    after_chars: usize,
    trimmed_chars: usize,
}

impl Text {
    fn character(&mut self, c: char, cap: usize) {
        if self.prefix_chars < cap {
            self.prefix.push(c);
            self.prefix_chars += 1;
        }
        self.chars += 1;
        if self.chars - 1 == self.leading && c.is_whitespace() {
            self.leading += 1;
        } else {
            if self.after_chars < cap {
                self.after_leading.push(c);
                self.after_chars += 1;
            }
            if !c.is_whitespace() {
                self.trimmed_chars = self.chars - self.leading;
            }
        }
    }

    fn append(&mut self, other: Text, cap: usize) {
        if other.chars == 0 {
            return;
        }
        let separator = usize::from(self.chars > 0);
        if separator != 0 {
            append_prefix(&mut self.prefix, &mut self.prefix_chars, "\n", cap);
        }
        append_prefix(&mut self.prefix, &mut self.prefix_chars, &other.prefix, cap);
        if self.chars == self.leading {
            if other.trimmed_chars == 0 {
                self.leading += separator + other.chars;
            } else {
                self.leading += separator + other.leading;
                append_prefix(
                    &mut self.after_leading,
                    &mut self.after_chars,
                    &other.after_leading,
                    cap,
                );
            }
        } else {
            if separator != 0 {
                append_prefix(&mut self.after_leading, &mut self.after_chars, "\n", cap);
            }
            append_prefix(
                &mut self.after_leading,
                &mut self.after_chars,
                &other.prefix,
                cap,
            );
        }
        if other.trimmed_chars != 0 {
            self.trimmed_chars =
                self.chars + separator + other.leading + other.trimmed_chars - self.leading;
        }
        self.chars += separator + other.chars;
    }
}

fn append_prefix(out: &mut String, used: &mut usize, text: &str, cap: usize) {
    for c in text.chars().take(cap - *used) {
        out.push(c);
        *used += 1;
    }
}

/// A string-token adapter, not a JSON parser: serde still validates objects/arrays/scalars and
/// skips unknown fields. The adapter validates every original string (including omitted suffixes),
/// emits only its bounded prefix, and hands its full text counts to the content visitor.
struct CappedJson<'a, R> {
    input: R,
    token: Vec<u8>,
    at: usize,
    text: &'a RefCell<Text>,
    cap: usize,
}

impl<R: Read> CappedJson<'_, R> {
    fn byte(&mut self) -> io::Result<u8> {
        let mut b = [0];
        self.input.read_exact(&mut b)?;
        Ok(b[0])
    }

    fn hex(&mut self) -> io::Result<u32> {
        let mut value = 0;
        for _ in 0..4 {
            let digit = char::from(self.byte()?)
                .to_digit(16)
                .ok_or_else(invalid_string)?;
            value = (value << 4) | digit;
        }
        Ok(value)
    }

    fn string(&mut self) -> io::Result<()> {
        let mut text = Text::default();
        loop {
            let first = self.byte()?;
            let c = match first {
                b'"' => break,
                b'\\' => match self.byte()? {
                    b'"' => '"',
                    b'\\' => '\\',
                    b'/' => '/',
                    b'b' => '\u{8}',
                    b'f' => '\u{c}',
                    b'n' => '\n',
                    b'r' => '\r',
                    b't' => '\t',
                    b'u' => {
                        let mut value = self.hex()?;
                        if (0xd800..=0xdbff).contains(&value) {
                            if self.byte()? != b'\\' || self.byte()? != b'u' {
                                return Err(invalid_string());
                            }
                            let low = self.hex()?;
                            if !(0xdc00..=0xdfff).contains(&low) {
                                return Err(invalid_string());
                            }
                            value = 0x10000 + ((value - 0xd800) << 10) + low - 0xdc00;
                        }
                        char::from_u32(value).ok_or_else(invalid_string)?
                    }
                    _ => return Err(invalid_string()),
                },
                0..=0x1f => return Err(invalid_string()),
                0x20..=0x7f => char::from(first),
                _ => {
                    let width = match first {
                        0xc2..=0xdf => 2,
                        0xe0..=0xef => 3,
                        0xf0..=0xf4 => 4,
                        _ => return Err(invalid_string()),
                    };
                    let mut bytes = [first, 0, 0, 0];
                    self.input.read_exact(&mut bytes[1..width])?;
                    std::str::from_utf8(&bytes[..width])
                        .ok()
                        .and_then(|s| s.chars().next())
                        .ok_or_else(invalid_string)?
                }
            };
            text.character(c, self.cap);
        }
        self.token.clear();
        serde_json::to_writer(&mut self.token, &text.prefix)?;
        *self.text.borrow_mut() = text;
        self.at = 0;
        Ok(())
    }
}

fn invalid_string() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid JSON string")
}

impl<R: Read> Read for CappedJson<'_, R> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        if self.at == self.token.len() {
            self.token.clear();
            self.at = 0;
            let n = self.input.read(&mut out[..1])?;
            if n == 0 || out[0] != b'"' {
                return Ok(n);
            }
            self.string()?;
        }
        let n = out.len().min(self.token.len() - self.at);
        out[..n].copy_from_slice(&self.token[self.at..self.at + n]);
        self.at += n;
        Ok(n)
    }
}

#[derive(Default)]
struct Entry {
    kind: Option<String>,
    message: Option<Message>,
}
#[derive(Default)]
struct Message {
    role: Option<String>,
    stop: Option<String>,
    error: Option<String>,
    content: Text,
}

struct EntrySeed<'a> {
    text: &'a RefCell<Text>,
    cap: usize,
}
impl<'de> DeserializeSeed<'de> for EntrySeed<'_> {
    type Value = Entry;
    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<Entry, D::Error> {
        d.deserialize_map(self)
    }
}
impl<'de> Visitor<'de> for EntrySeed<'_> {
    type Value = Entry;
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("an omp JSONL entry")
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Entry, A::Error> {
        let mut entry = Entry::default();
        while let Some(key) = map.next_key::<String>()? {
            match key.as_str() {
                "type" => entry.kind = map.next_value()?,
                "message" => {
                    entry.message = Some(map.next_value_seed(MessageSeed {
                        text: self.text,
                        cap: self.cap,
                    })?)
                }
                _ => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }
        Ok(entry)
    }
}

struct MessageSeed<'a> {
    text: &'a RefCell<Text>,
    cap: usize,
}
impl<'de> DeserializeSeed<'de> for MessageSeed<'_> {
    type Value = Message;
    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<Message, D::Error> {
        d.deserialize_map(self)
    }
}
impl<'de> Visitor<'de> for MessageSeed<'_> {
    type Value = Message;
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("an omp message")
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Message, A::Error> {
        let mut message = Message::default();
        while let Some(key) = map.next_key::<String>()? {
            match key.as_str() {
                "role" => message.role = map.next_value()?,
                "stopReason" => message.stop = map.next_value()?,
                "errorMessage" => message.error = map.next_value()?,
                "content" => {
                    message.content = map.next_value_seed(ContentSeed {
                        text: self.text,
                        cap: self.cap,
                    })?
                }
                _ => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }
        Ok(message)
    }
}

struct ContentSeed<'a> {
    text: &'a RefCell<Text>,
    cap: usize,
}
impl<'de> DeserializeSeed<'de> for ContentSeed<'_> {
    type Value = Text;
    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<Text, D::Error> {
        d.deserialize_any(self)
    }
}
impl<'de> Visitor<'de> for ContentSeed<'_> {
    type Value = Text;
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("text or omp message content parts")
    }
    fn visit_str<E: serde::de::Error>(self, _value: &str) -> Result<Text, E> {
        Ok(std::mem::take(&mut *self.text.borrow_mut()))
    }
    fn visit_unit<E: serde::de::Error>(self) -> Result<Text, E> {
        Ok(Text::default())
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Text, A::Error> {
        let mut text = Text::default();
        while let Some((part, captured)) = seq.next_element_seed(PartSeed { text: self.text })? {
            if part.text_part().is_some() {
                text.append(captured, self.cap);
            }
        }
        Ok(text)
    }
}

struct PartSeed<'a> {
    text: &'a RefCell<Text>,
}
impl<'de> DeserializeSeed<'de> for PartSeed<'_> {
    type Value = (Part<'static>, Text);
    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<Self::Value, D::Error> {
        d.deserialize_map(self)
    }
}
impl<'de> Visitor<'de> for PartSeed<'_> {
    type Value = (Part<'static>, Text);
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("an omp content part")
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut part = Part {
            kind: None,
            text: None,
        };
        let mut captured = Text::default();
        while let Some(key) = map.next_key::<String>()? {
            match key.as_str() {
                "type" => part.kind = map.next_value::<Option<String>>()?.map(Cow::Owned),
                "text" => {
                    part.text = map.next_value::<Option<String>>()?.map(Cow::Owned);
                    if part.text.is_some() {
                        captured = std::mem::take(&mut *self.text.borrow_mut());
                    }
                }
                _ => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }
        Ok((part, captured))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use std::io::Write;

    fn user(text: &str) -> Value {
        json!({"type":"message","message":{"role":"user","content":text}})
    }

    fn assistant(reason: &str, text: Option<&str>) -> Value {
        json!({"type":"message","message":{"role":"assistant","stopReason":reason,
            "content":text.map(|text| vec![json!({"type":"text","text":text})])}})
    }

    fn append(path: &std::path::Path, entries: &[Value]) {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        for entry in entries {
            serde_json::to_writer(&mut file, entry).unwrap();
            file.write_all(b"\n").unwrap();
        }
    }

    #[test]
    fn exact_prompt_beats_the_first_user_but_transformed_prompts_fall_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        append(&path, &[user("old"), assistant("stop", Some("old reply"))]);
        let from = std::fs::metadata(&path).unwrap().len();
        append(
            &path,
            &[
                user("transformed"),
                assistant("stop", Some("fallback reply")),
            ],
        );
        let mut watch = TurnWatch::new(path.clone(), from, Some(" \twanted\n".into()));
        assert!(
            !watch.matched_exactly(),
            "no user entry has been scanned yet"
        );
        let fallback = watch.poll().unwrap();
        assert!(
            !watch.matched_exactly(),
            "the first user entry is only a fallback"
        );
        assert!(fallback.prompt_seen);
        assert_eq!(fallback.reply.as_deref(), Some("fallback reply"));
        assert_eq!(fallback.end, Some(TurnOutcome::Completed));
        append(
            &path,
            &[user("\n wanted  "), assistant("stop", Some("exact reply"))],
        );
        let exact = watch.poll().unwrap();
        assert!(
            watch.matched_exactly(),
            "the exact prompt supersedes the fallback"
        );
        assert_eq!(exact.reply.as_deref(), Some("exact reply"));
        assert!(!exact.later_prompt);
        assert_eq!(watch.poll().unwrap(), exact, "unchanged poll is idempotent");

        let mut fallback =
            TurnWatch::new(path, from, Some("a slash command's original text".into()));
        let scan = fallback.poll().unwrap();
        assert!(
            !fallback.matched_exactly(),
            "a transformed prompt remains a fallback"
        );
        assert_eq!(scan.reply.as_deref(), Some("fallback reply"));
        assert!(scan.later_prompt);
    }

    #[test]
    fn tool_use_is_not_terminal_and_last_text_survives_a_textless_end() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        append(
            &path,
            &[user("work"), assistant("toolUse", Some("First progress"))],
        );
        let mut watch = TurnWatch::new(path.clone(), 0, None);
        let progress = watch.poll().unwrap();
        assert_eq!(progress.end, None);
        assert_eq!(progress.reply.as_deref(), Some("First progress"));
        append(
            &path,
            &[
                assistant("toolUse", Some("Latest progress")),
                assistant("stop", None),
            ],
        );
        let scan = watch.poll().unwrap();
        assert_eq!(scan.end, Some(TurnOutcome::Completed));
        assert_eq!(scan.reply.as_deref(), Some("Latest progress"));
    }

    #[test]
    fn error_and_abort_use_the_existing_turn_outcome_rules() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let mut error = assistant("error", None);
        error["message"]["errorMessage"] = json!(format!("  {}  ", "é".repeat(300)));
        append(&path, &[user("error"), error]);
        let mut watch = TurnWatch::new(path.clone(), 0, None);
        assert_eq!(
            watch.poll().unwrap().end,
            Some(TurnOutcome::Error(Some("é".repeat(200))))
        );
        append(&path, &[user("abort"), assistant("aborted", None)]);
        let mut watch = TurnWatch::new(path, 0, Some("abort".into()));
        assert_eq!(watch.poll().unwrap().end, Some(TurnOutcome::Aborted));
    }

    #[test]
    fn later_prompt_freezes_the_attributed_span() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        append(
            &path,
            &[
                user("mine"),
                assistant("stop", Some("My reply")),
                user("another"),
                assistant("error", Some("Not my reply")),
            ],
        );
        let mut watch = TurnWatch::new(path, 0, Some("mine".into()));
        let scan = watch.poll().unwrap();
        assert!(scan.later_prompt);
        assert_eq!(scan.end, Some(TurnOutcome::Completed));
        assert_eq!(scan.reply.as_deref(), Some("My reply"));
    }

    #[test]
    fn reply_cap_counts_unicode_characters_across_content_parts() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let head = "🦀".repeat(REPLY_CHARS + 5);
        let entry = json!({"type":"message","message":{"role":"assistant","stopReason":"stop","content":[
            {"type":"thinking","text":"not reply text"}, {"type":"text","text":head},
            {"type":"image","data":"x".repeat(2 * READ_BYTES)}, {"text":"last","type":"text"}
        ]}});
        append(&path, &[user("work"), entry]);
        let mut watch = TurnWatch::new(path, 0, None);
        let scan = watch.poll().unwrap();
        assert_eq!(
            scan.reply.as_deref(),
            Some("🦀".repeat(REPLY_CHARS).as_str())
        );
        assert_eq!(scan.reply_more, 5 + 1 + 4);
    }

    #[test]
    fn matching_handles_long_trimmed_whitespace_and_multiple_text_parts() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let parts = json!({"type":"message","message":{"role":"user","content":[
            {"text":format!("{}first", " ".repeat(REPLY_CHARS * 2)), "type":"text"},
            {"type":"text","text":format!("second{}", "\t".repeat(REPLY_CHARS * 2))}
        ]}});
        append(
            &path,
            &[
                user("fallback"),
                assistant("stop", Some("not mine")),
                parts,
                assistant("stop", Some("matched")),
            ],
        );
        let mut watch = TurnWatch::new(path, 0, Some("first\nsecond".into()));
        assert_eq!(watch.poll().unwrap().reply.as_deref(), Some("matched"));
    }

    #[test]
    fn incremental_polls_keep_a_half_written_line_including_split_utf8() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        append(&path, &[user("work")]);
        let mut watch = TurnWatch::new(path.clone(), 0, Some("work".into()));
        let before = watch.poll().unwrap();
        let mut line = serde_json::to_vec(&assistant("stop", Some("é done"))).unwrap();
        line.push(b'\n');
        let cut = line.iter().position(|b| *b == 0xc3).unwrap() + 1;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        file.write_all(&line[..cut]).unwrap();
        assert_eq!(watch.poll().unwrap(), before);
        assert_eq!(watch.poll().unwrap(), before);
        let start = watch.line_start;
        file.write_all(&line[cut..line.len() - 1]).unwrap();
        assert_eq!(
            watch.poll().unwrap(),
            before,
            "valid JSON without a newline is incomplete"
        );
        assert_eq!(watch.line_start, start);
        file.write_all(b"\n").unwrap();
        let complete = watch.poll().unwrap();
        assert_eq!(complete.reply.as_deref(), Some("é done"));
        assert_eq!(complete.end, Some(TurnOutcome::Completed));
    }

    #[test]
    fn shrink_invalidates_even_an_observed_partial_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        append(&path, &[user("work")]);
        let mut watch = TurnWatch::new(path.clone(), 0, None);
        watch.poll().unwrap();
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        file.write_all(b"{\"half\":").unwrap();
        watch.poll().unwrap();
        file.set_len(watch.line_start).unwrap();
        let error = watch.poll().unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("rewritten"));
    }

    #[test]
    fn giant_tokens_are_bounded_and_invalid_omitted_suffixes_are_rejected() {
        let huge = format!("\"{}\\uD83E\\uDD80\"", "x".repeat(2 * READ_BYTES));
        let text = RefCell::new(Text::default());
        let mut decoder = CappedJson {
            input: huge.as_bytes(),
            token: Vec::new(),
            at: 0,
            text: &text,
            cap: REPLY_CHARS,
        };
        let value: String = serde_json::from_reader(&mut decoder).unwrap();
        assert_eq!(value.len(), REPLY_CHARS);
        assert_eq!(text.borrow().chars, 2 * READ_BYTES + 1);
        assert!(decoder.token.capacity() < READ_BYTES * 2);
        let bad = format!("\"{}\\q\"", "x".repeat(2 * READ_BYTES));
        let decoder = CappedJson {
            input: bad.as_bytes(),
            token: Vec::new(),
            at: 0,
            text: &text,
            cap: REPLY_CHARS,
        };
        assert!(serde_json::from_reader::<_, String>(decoder).is_err());
    }
}
