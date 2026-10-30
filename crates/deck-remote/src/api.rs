//! The device application protocol v1 (`docs/notes/mobile-relay.md`, "R4 contract").
//!
//! Runs inside an established Noise reconnect session. One application message (at most
//! [`MAX_APP_MESSAGE`] bytes, both directions) is a non-empty sequence of **records**:
//! `kind: u8 | len: u32 BE | body[len]` ([`Record`], [`encode`], [`decode`]). Control records
//! carry JSON ([`Request`], [`Reply`], [`Event`], [`Welcome`], [`TermEnd`]); terminal records
//! are binary.
//!
//! JSON conventions: tagged enums are internally tagged (`op`, `feed`, `type`, `state`, `code`,
//! `reason`), `snake_case`, struct variants only; plain value enums are strings. `None` is
//! written as `null`; device → gateway types also accept a missing optional field and refuse
//! unknown fields. A device ignores fields and record kinds it does not know. Every timestamp
//! is milliseconds since the Unix epoch in a field named `*_unix_ms`. Every fractional number
//! is an `Option<f64>`: `null` when unknown or not finite (JSON has no NaN or infinity).
//!
//! A request that is well-formed JSON with a numeric `id` but names an op, feed, session
//! action type or enum value ([`ViewMode`], [`DiffSide`], [`CompactMode`]) this gateway does
//! not know is answered [`ApiError::Unsupported`], any other bad request
//! [`ApiError::BadRequest`] ([`Request::parse`]): a newer device probing an older gateway can
//! tell the two apart.
//!
//! Record kinds are partitioned by sender: device kinds are `0x01..=0x0f`
//! ([`kind::DEVICE`]), gateway kinds `0x10..=0x7f` ([`kind::GATEWAY`]); `0x00` and
//! `0x80..=0xff` are reserved. A receiver refuses a kind of the other side's range
//! ([`RecordError::WrongDirection`]) and a reserved kind ([`RecordError::UnknownKind`]); the
//! gateway refuses an unknown device kind, a device skips an unknown gateway kind.
//!
//! Terminal records carry the session's raw output. The device's terminal emulator answers no
//! queries and sends no reports (no DA, DSR, OSC 10/11/4 colour queries, title or size reports:
//! its only input is what the user types) and ignores OSC 52 (clipboard). The gateway does not
//! strip these sequences from the output.
//!
//! Devices never see absolute paths, ctl keys, other clients' request keys or secrets: projects
//! are [`project_id`]s, sessions omp session ids, live sessions [`LiveRef`]s and changed files
//! repo-relative paths.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Device application protocol version ([`Welcome::protocol`]).
pub const PROTOCOL: u32 = 1;
/// Largest application message (Noise plaintext), both directions.
pub const MAX_APP_MESSAGE: usize = 1 << 20;
/// `kind` + `len` in front of every record body.
pub const RECORD_HEADER: usize = 5;
/// Largest JSON reply body: what one application message carries after a record header.
pub const MAX_REPLY: usize = MAX_APP_MESSAGE - RECORD_HEADER;
/// Bytes of one `INPUT` record (after `view` and `flags`).
pub const MAX_INPUT: usize = 64 * 1024;
/// Bytes of one `T_SNAPSHOT` chunk (after `view` and `last`).
pub const MAX_SNAPSHOT_CHUNK: usize = 256 * 1024;
/// A snapshot above this ends its view with [`TermEnd::TooLarge`].
pub const MAX_SNAPSHOT: usize = 16 << 20;

/// [`Limits`] defaults.
pub const WINDOW_BYTES: u32 = 1 << 20;
pub const WINDOW_MESSAGES: u32 = 32;
pub const MAX_INFLIGHT: u32 = 32;
pub const MAX_VIEWS: u32 = 4;
pub const MAX_FEEDS: u32 = 16;

/// A device acknowledges at least every this many messages, bytes or milliseconds.
pub const ACK_EVERY_MESSAGES: u64 = 8;
pub const ACK_EVERY_BYTES: u64 = 256 * 1024;
pub const ACK_EVERY_MS: u64 = 100;
/// No ack for this long while messages are outstanding closes the device (`close::STALLED`).
pub const STALL_SECS: u64 = 60;
/// Gateway egress over all devices.
pub const EGRESS_BYTES_PER_SEC: u64 = 6 << 20;
pub const EGRESS_FRAMES_PER_SEC: u64 = 3000;
/// Per-device event queue (outcome/progress/confirm/…): overflow clears it and sends one
/// [`Event::Resync`].
pub const EVENT_QUEUE: usize = 1024;
pub const EVENT_BYTES: usize = 4 << 20;

/// [`Feed::Transcript`] `tail` at most.
pub const MAX_TAIL: u32 = 500;
/// [`Feed::Sessions`] `limit` range.
pub const MAX_SESSIONS_LIMIT: u32 = 500;
/// [`Feed::Cards`] sessions at most.
pub const MAX_CARDS: usize = 64;
/// [`SessionSummary::first_prompt`] and [`SessionSummary::title`] characters at most.
pub const FIRST_PROMPT_CHARS: usize = 200;

/// Every record fits one message, so every reply and feed value is bounded by design. The
/// gateway serializes a value and, when it would exceed [`MAX_REPLY`], drops the oldest
/// entries (lists are ordered newest/most relevant first) and sets the value's `truncated`
/// flag; nothing is ever sent oversize or dropped without that flag:
/// - `projects`, `live`, `inbox`, `cards`, `changes` values and `models` replies: whole
///   entries dropped from the end, `truncated: true`.
/// - `sessions`: at most `limit` entries (`total` tells how many exist), then the same rule.
/// - `transcript`: a value carries at most [`MAX_TRANSCRIPT_BYTES`] of items; the rest follows
///   as further appends (`start` continues). An item's text fields are cut to
///   [`MAX_ITEM_TEXT`] bytes (at a character boundary) and end with [`CUT_MARK`].
/// - `diff` replies: the text is cut to [`MAX_DIFF_TEXT`] bytes with `truncated: true`.
pub const MAX_TRANSCRIPT_BYTES: usize = 512 * 1024;
/// Bytes of one transcript item's text field at most (then [`CUT_MARK`]).
pub const MAX_ITEM_TEXT: usize = 64 * 1024;
/// Ends a text the gateway cut.
pub const CUT_MARK: &str = "…";
/// Bytes of a diff text at most.
pub const MAX_DIFF_TEXT: usize = 768 * 1024;

/// Record kinds.
pub mod kind {
    use std::ops::RangeInclusive;

    /// Every device → gateway kind, present and future.
    pub const DEVICE: RangeInclusive<u8> = 0x01..=0x0f;
    /// Every gateway → device kind, present and future.
    pub const GATEWAY: RangeInclusive<u8> = 0x10..=0x7f;

    /// Device → gateway: JSON [`super::Request`].
    pub const REQUEST: u8 = 0x01;
    /// Device → gateway: `messages: u64 BE`.
    pub const ACK: u8 = 0x02;
    /// Device → gateway: `view: u32 BE | flags: u8 | bytes`.
    pub const INPUT: u8 = 0x03;
    /// Device → gateway: `view: u32 | cols: u16 | rows: u16` (BE).
    pub const RESIZE: u8 = 0x04;
    /// Gateway → device: JSON [`super::Welcome`], the first record of a session.
    pub const WELCOME: u8 = 0x10;
    /// Gateway → device: JSON [`super::Reply`].
    pub const REPLY: u8 = 0x11;
    /// Gateway → device: JSON [`super::Event`].
    pub const EVENT: u8 = 0x12;
    /// Gateway → device: `view: u32 | last: u8 | bytes`.
    pub const T_SNAPSHOT: u8 = 0x20;
    /// Gateway → device: `view: u32 | bytes`.
    pub const T_OUTPUT: u8 = 0x21;
    /// Gateway → device: `view: u32 | cols: u16 | rows: u16`.
    pub const T_RESIZE: u8 = 0x22;
    /// Gateway → device: `view: u32 | JSON` [`super::TermEnd`].
    pub const T_END: u8 = 0x23;
}

/// `INPUT` flag bit 0: the bytes are a Ctrl+C.
pub const INPUT_CTRL_C: u8 = 1;

// ---- records ------------------------------------------------------------------------------------

/// One record. JSON bodies stay bytes here: decoding them is a separate, typed step
/// ([`Request::parse`], `serde_json`), so a bad request can be answered instead of closing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Record {
    Request(Vec<u8>),
    /// Cumulative count of gateway messages the device processed.
    Ack {
        messages: u64,
    },
    Input {
        view: u32,
        ctrl_c: bool,
        bytes: Vec<u8>,
    },
    Resize {
        view: u32,
        cols: u16,
        rows: u16,
    },
    Welcome(Vec<u8>),
    Reply(Vec<u8>),
    Event(Vec<u8>),
    Snapshot {
        view: u32,
        last: bool,
        bytes: Vec<u8>,
    },
    Output {
        view: u32,
        bytes: Vec<u8>,
    },
    TermResize {
        view: u32,
        cols: u16,
        rows: u16,
    },
    /// JSON [`TermEnd`].
    TermEnd {
        view: u32,
        end: Vec<u8>,
    },
}

/// Who sent a message being decoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
    /// Strict: only device kinds, nothing unknown.
    Device,
    /// Gateway kinds; unknown ones of [`kind::GATEWAY`] are skipped (a newer gateway), device
    /// and reserved kinds refused.
    Gateway,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RecordError {
    #[error("empty application message")]
    Empty,
    #[error("application message of {0} bytes exceeds {MAX_APP_MESSAGE}")]
    TooLarge(usize),
    #[error("truncated record")]
    Truncated,
    #[error("unknown record kind {0:#04x}")]
    UnknownKind(u8),
    #[error("record kind {0:#04x} is not sent by this side")]
    WrongDirection(u8),
    #[error("malformed record of kind {0:#04x}")]
    Malformed(u8),
    #[error("record of kind {kind:#04x} carries {len} bytes, more than {max}")]
    Oversized { kind: u8, len: usize, max: usize },
}

impl Record {
    pub fn kind(&self) -> u8 {
        match self {
            Record::Request(_) => kind::REQUEST,
            Record::Ack { .. } => kind::ACK,
            Record::Input { .. } => kind::INPUT,
            Record::Resize { .. } => kind::RESIZE,
            Record::Welcome(_) => kind::WELCOME,
            Record::Reply(_) => kind::REPLY,
            Record::Event(_) => kind::EVENT,
            Record::Snapshot { .. } => kind::T_SNAPSHOT,
            Record::Output { .. } => kind::T_OUTPUT,
            Record::TermResize { .. } => kind::T_RESIZE,
            Record::TermEnd { .. } => kind::T_END,
        }
    }

    fn body_len(&self) -> usize {
        match self {
            Record::Request(b) | Record::Welcome(b) | Record::Reply(b) | Record::Event(b) => {
                b.len()
            }
            Record::Ack { .. } | Record::Resize { .. } | Record::TermResize { .. } => 8,
            Record::Input { bytes, .. } | Record::Snapshot { bytes, .. } => 5 + bytes.len(),
            Record::Output { bytes, .. } => 4 + bytes.len(),
            Record::TermEnd { end, .. } => 4 + end.len(),
        }
    }

    /// Header plus body.
    pub fn encoded_len(&self) -> usize {
        RECORD_HEADER + self.body_len()
    }

    /// Appends the record; refuses bodies over their limits.
    pub fn encode_into(&self, out: &mut Vec<u8>) -> Result<(), RecordError> {
        let kind = self.kind();
        let oversized = |len: usize, max: usize| {
            if len > max {
                Err(RecordError::Oversized { kind, len, max })
            } else {
                Ok(())
            }
        };
        match self {
            Record::Input { bytes, .. } => oversized(bytes.len(), MAX_INPUT)?,
            Record::Snapshot { bytes, .. } => oversized(bytes.len(), MAX_SNAPSHOT_CHUNK)?,
            _ => {}
        }
        let len = self.body_len();
        oversized(len, MAX_APP_MESSAGE - RECORD_HEADER)?;
        out.reserve(RECORD_HEADER + len);
        out.push(kind);
        out.extend_from_slice(&(len as u32).to_be_bytes());
        match self {
            Record::Request(b) | Record::Welcome(b) | Record::Reply(b) | Record::Event(b) => {
                out.extend_from_slice(b)
            }
            Record::Ack { messages } => out.extend_from_slice(&messages.to_be_bytes()),
            Record::Input {
                view,
                ctrl_c,
                bytes,
            } => {
                out.extend_from_slice(&view.to_be_bytes());
                out.push(if *ctrl_c { INPUT_CTRL_C } else { 0 });
                out.extend_from_slice(bytes);
            }
            Record::Resize { view, cols, rows } | Record::TermResize { view, cols, rows } => {
                out.extend_from_slice(&view.to_be_bytes());
                out.extend_from_slice(&cols.to_be_bytes());
                out.extend_from_slice(&rows.to_be_bytes());
            }
            Record::Snapshot { view, last, bytes } => {
                out.extend_from_slice(&view.to_be_bytes());
                out.push(u8::from(*last));
                out.extend_from_slice(bytes);
            }
            Record::Output { view, bytes } => {
                out.extend_from_slice(&view.to_be_bytes());
                out.extend_from_slice(bytes);
            }
            Record::TermEnd { view, end } => {
                out.extend_from_slice(&view.to_be_bytes());
                out.extend_from_slice(end);
            }
        }
        Ok(())
    }

    fn device_kind(kind: u8) -> bool {
        matches!(kind, kind::REQUEST | kind::ACK | kind::INPUT | kind::RESIZE)
    }

    fn gateway_kind(kind: u8) -> bool {
        matches!(
            kind,
            kind::WELCOME
                | kind::REPLY
                | kind::EVENT
                | kind::T_SNAPSHOT
                | kind::T_OUTPUT
                | kind::T_RESIZE
                | kind::T_END
        )
    }

    /// `None`: an unknown gateway kind (skipped).
    fn decode_one(kind: u8, body: &[u8], origin: Origin) -> Result<Option<Record>, RecordError> {
        let (mine, theirs, known) = match origin {
            Origin::Device => (kind::DEVICE, kind::GATEWAY, Record::device_kind(kind)),
            Origin::Gateway => (kind::GATEWAY, kind::DEVICE, Record::gateway_kind(kind)),
        };
        if theirs.contains(&kind) {
            return Err(RecordError::WrongDirection(kind));
        }
        if !mine.contains(&kind) {
            // `0x00` and `0x80..=0xff`: reserved.
            return Err(RecordError::UnknownKind(kind));
        }
        if !known {
            return match origin {
                Origin::Device => Err(RecordError::UnknownKind(kind)),
                Origin::Gateway => Ok(None),
            };
        }
        let malformed = || RecordError::Malformed(kind);
        let view = |b: &[u8]| -> Result<u32, RecordError> {
            let head: [u8; 4] = b
                .get(..4)
                .and_then(|h| h.try_into().ok())
                .ok_or_else(malformed)?;
            Ok(u32::from_be_bytes(head))
        };
        let geometry = |b: &[u8]| -> Result<(u32, u16, u16), RecordError> {
            if b.len() != 8 {
                return Err(malformed());
            }
            Ok((
                view(b)?,
                u16::from_be_bytes([b[4], b[5]]),
                u16::from_be_bytes([b[6], b[7]]),
            ))
        };
        let record = match kind {
            kind::REQUEST => Record::Request(body.to_vec()),
            kind::ACK => {
                let b: [u8; 8] = body.try_into().map_err(|_| malformed())?;
                Record::Ack {
                    messages: u64::from_be_bytes(b),
                }
            }
            kind::INPUT => {
                let view = view(body)?;
                let flags = *body.get(4).ok_or_else(malformed)?;
                if flags & !INPUT_CTRL_C != 0 {
                    return Err(malformed());
                }
                let bytes = &body[5..];
                if bytes.len() > MAX_INPUT {
                    return Err(RecordError::Oversized {
                        kind,
                        len: bytes.len(),
                        max: MAX_INPUT,
                    });
                }
                Record::Input {
                    view,
                    ctrl_c: flags & INPUT_CTRL_C != 0,
                    bytes: bytes.to_vec(),
                }
            }
            kind::RESIZE => {
                let (view, cols, rows) = geometry(body)?;
                Record::Resize { view, cols, rows }
            }
            kind::WELCOME => Record::Welcome(body.to_vec()),
            kind::REPLY => Record::Reply(body.to_vec()),
            kind::EVENT => Record::Event(body.to_vec()),
            kind::T_SNAPSHOT => {
                let view = view(body)?;
                let last = match body.get(4) {
                    Some(0) => false,
                    Some(1) => true,
                    _ => return Err(malformed()),
                };
                let bytes = &body[5..];
                if bytes.len() > MAX_SNAPSHOT_CHUNK {
                    return Err(RecordError::Oversized {
                        kind,
                        len: bytes.len(),
                        max: MAX_SNAPSHOT_CHUNK,
                    });
                }
                Record::Snapshot {
                    view,
                    last,
                    bytes: bytes.to_vec(),
                }
            }
            kind::T_OUTPUT => Record::Output {
                view: view(body)?,
                bytes: body[4..].to_vec(),
            },
            kind::T_RESIZE => {
                let (view, cols, rows) = geometry(body)?;
                Record::TermResize { view, cols, rows }
            }
            kind::T_END => Record::TermEnd {
                view: view(body)?,
                end: body[4..].to_vec(),
            },
            other => return Err(RecordError::UnknownKind(other)),
        };
        Ok(Some(record))
    }
}

/// One application message holding `records` in order.
pub fn encode(records: &[Record]) -> Result<Vec<u8>, RecordError> {
    if records.is_empty() {
        return Err(RecordError::Empty);
    }
    let len: usize = records.iter().map(Record::encoded_len).sum();
    if len > MAX_APP_MESSAGE {
        return Err(RecordError::TooLarge(len));
    }
    let mut out = Vec::with_capacity(len);
    for r in records {
        r.encode_into(&mut out)?;
    }
    Ok(out)
}

/// The records of one application message: a whole sequence or an error.
pub fn decode(message: &[u8], origin: Origin) -> Result<Vec<Record>, RecordError> {
    if message.is_empty() {
        return Err(RecordError::Empty);
    }
    if message.len() > MAX_APP_MESSAGE {
        return Err(RecordError::TooLarge(message.len()));
    }
    let mut out = Vec::new();
    let mut rest = message;
    while !rest.is_empty() {
        let head: [u8; RECORD_HEADER] = rest
            .get(..RECORD_HEADER)
            .and_then(|h| h.try_into().ok())
            .ok_or(RecordError::Truncated)?;
        let len = u32::from_be_bytes([head[1], head[2], head[3], head[4]]) as usize;
        let end = RECORD_HEADER
            .checked_add(len)
            .ok_or(RecordError::Truncated)?;
        let body = rest.get(RECORD_HEADER..end).ok_or(RecordError::Truncated)?;
        if let Some(record) = Record::decode_one(head[0], body, origin)? {
            out.push(record);
        }
        rest = &rest[end..];
    }
    Ok(out)
}

// ---- ids ----------------------------------------------------------------------------------------

/// `"p-"` + the first 16 hex digits of SHA-256 over the cwd's UTF-8 (lossy) bytes.
pub fn project_id(cwd_bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(cwd_bytes);
    let digest = Sha256::digest(text.as_bytes());
    let mut id = String::with_capacity(18);
    id.push_str("p-");
    for b in &digest[..8] {
        id.push_str(&format!("{b:02x}"));
    }
    id
}

/// A controlled session: the gateway's session key (opaque) and its generation. The epoch is
/// the link's ([`Welcome::epoch`]).
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LiveRef {
    pub key: String,
    pub generation: u64,
}

// ---- session ------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Permissions {
    pub control: bool,
    pub manage: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Limits {
    pub max_message: u32,
    pub window_bytes: u32,
    pub window_messages: u32,
    pub max_inflight: u32,
    pub max_views: u32,
    pub max_feeds: u32,
}

impl Default for Limits {
    fn default() -> Limits {
        Limits {
            max_message: MAX_APP_MESSAGE as u32,
            window_bytes: WINDOW_BYTES,
            window_messages: WINDOW_MESSAGES,
            max_inflight: MAX_INFLIGHT,
            max_views: MAX_VIEWS,
            max_feeds: MAX_FEEDS,
        }
    }
}

/// The first record of every session.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Welcome {
    pub protocol: u32,
    pub gateway_version: String,
    /// The gateway instance (32 hex); live refs and seqs are bound to it.
    pub epoch: String,
    /// This device's registry id (`d-<16 hex>`): the client of its requests.
    pub device: String,
    pub name: String,
    pub permissions: Permissions,
    /// New journaled ops use `seq >= next_seq`; lower seqs only for `outcome` lookups.
    pub next_seq: u64,
    pub limits: Limits,
    pub features: Vec<String>,
}

// ---- requests -----------------------------------------------------------------------------------

/// What an op needs beyond the session itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Class {
    View,
    Control,
    Manage,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub id: u64,
    /// Required exactly for journaled ops ([`Op::journaled`]).
    #[serde(default)]
    pub seq: Option<u64>,
    pub op: Op,
}

/// Why a `REQUEST` record could not be parsed.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RequestError {
    /// Not a JSON object with a numeric `id`: the session is closed (`close::APP_PROTOCOL`).
    #[error("unreadable request")]
    Unreadable,
    /// An op, feed, action type or enum value this gateway does not know: answered
    /// [`ApiError::Unsupported`].
    #[error("unsupported operation")]
    Unsupported { id: u64 },
    /// Answered [`ApiError::BadRequest`].
    #[error("bad request: {message}")]
    Bad { id: u64, message: String },
}

impl Request {
    /// Strict parse of a `REQUEST` body.
    pub fn parse(body: &[u8]) -> Result<Request, RequestError> {
        match serde_json::from_slice::<Request>(body) {
            Ok(r) => Ok(r),
            Err(e) => {
                let value: serde_json::Value =
                    serde_json::from_slice(body).map_err(|_| RequestError::Unreadable)?;
                let id = value
                    .get("id")
                    .and_then(serde_json::Value::as_u64)
                    .ok_or(RequestError::Unreadable)?;
                if value.get("op").is_some_and(names_unknown) {
                    Err(RequestError::Unsupported { id })
                } else {
                    Err(RequestError::Bad {
                        id,
                        message: e.to_string(),
                    })
                }
            }
        }
    }

    /// The `seq` rule: present exactly for journaled ops.
    pub fn check_seq(&self) -> Result<(), String> {
        match (self.op.journaled(), self.seq) {
            (true, None) => Err(format!("{} needs a seq", self.op.name())),
            (false, Some(_)) => Err(format!("{} takes no seq", self.op.name())),
            _ => Ok(()),
        }
    }
}

/// The op names a tag or string enum value outside v1's sets: every place an [`Op`] holds a
/// tag or a plain enum (keep in step when one is added; `tests/api.rs` checks it against serde).
fn names_unknown(op: &serde_json::Value) -> bool {
    let unknown = |v: Option<&serde_json::Value>, names: &[&str]| matches!(v, Some(serde_json::Value::String(s)) if !names.contains(&s.as_str()));
    let Some(serde_json::Value::String(name)) = op.get("op") else {
        return false;
    };
    match name.as_str() {
        name if !Op::NAMES.contains(&name) => true,
        "subscribe" | "unsubscribe" => {
            unknown(op.get("feed").and_then(|f| f.get("feed")), Feed::NAMES)
        }
        "diff" => unknown(op.get("side"), DiffSide::NAMES),
        "attach" => unknown(op.get("mode"), ViewMode::NAMES),
        "action" => {
            let action = op.get("action");
            let kind = action.and_then(|a| a.get("type"));
            unknown(kind, SessionAction::NAMES)
                || (kind.and_then(serde_json::Value::as_str) == Some("compact")
                    && unknown(action.and_then(|a| a.get("mode")), CompactMode::NAMES))
        }
        _ => false,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ViewMode {
    /// Follows the output.
    Observe,
    /// Types; geometry and focus stay with the desktop.
    Input,
    /// Types and owns geometry (refused while another controlling view exists).
    Control,
}

impl ViewMode {
    /// Every value.
    pub const NAMES: &'static [&'static str] = &["observe", "input", "control"];
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffSide {
    Staged,
    Unstaged,
    Untracked,
}

impl DiffSide {
    /// Every value.
    pub const NAMES: &'static [&'static str] = &["staged", "unstaged", "untracked"];
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactMode {
    Default,
    Soft,
    Snapcompact,
    /// With [`SessionAction::Compact::focus`].
    Focus,
}

impl CompactMode {
    /// Every value.
    pub const NAMES: &'static [&'static str] = &["default", "soft", "snapcompact", "focus"];
}

/// A session runtime command ([`Op::Action`]).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum SessionAction {
    Handoff {
        #[serde(default)]
        focus: Option<String>,
    },
    Compact {
        mode: CompactMode,
        #[serde(default)]
        focus: Option<String>,
    },
    Rename {
        name: String,
    },
    AutoName {},
    Plan {},
    Retry {},
}

impl SessionAction {
    /// Every `type` tag.
    pub const NAMES: &'static [&'static str] =
        &["handoff", "compact", "rename", "auto_name", "plan", "retry"];
}

/// Operations v1 (tag `op`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Op {
    // View
    Subscribe {
        feed: Feed,
    },
    Unsubscribe {
        feed: Feed,
    },
    /// This device's own request `seq`.
    Outcome {
        seq: u64,
    },
    Models {},
    Diff {
        project: String,
        file: String,
        side: DiffSide,
    },
    /// `observe` is View; `input`/`control` are Control (never journaled).
    Attach {
        live: LiveRef,
        mode: ViewMode,
    },
    Detach {
        view: u32,
    },
    Ping {},
    // Control
    NewSession {
        project: String,
    },
    Resume {
        session: String,
    },
    Restart {
        live: LiveRef,
    },
    Stop {
        live: LiveRef,
    },
    Kill {
        live: LiveRef,
    },
    Prompt {
        live: LiveRef,
        text: String,
    },
    Interrupt {
        live: LiveRef,
    },
    SwitchModel {
        live: LiveRef,
        selector: String,
        #[serde(default)]
        level: Option<String>,
    },
    SetEffort {
        live: LiveRef,
        level: String,
    },
    Action {
        live: LiveRef,
        action: SessionAction,
    },
    /// A draft confirmation ([`Event::Confirm`]).
    Answer {
        live: LiveRef,
        request: u64,
        proceed: bool,
    },
    /// Cancel this device's running job `seq`.
    Cancel {
        seq: u64,
    },
    InboxOpen {
        live: LiveRef,
    },
    InboxDismiss {
        live: LiveRef,
    },
    InboxClear {},
    Stage {
        project: String,
        files: Vec<String>,
    },
    Unstage {
        project: String,
        files: Vec<String>,
    },
    // Manage
    /// Running ones are stopped first.
    DeleteSessions {
        sessions: Vec<String>,
    },
    CopySession {
        session: String,
        project: String,
    },
    /// The session is stopped first.
    MoveSession {
        session: String,
        project: String,
    },
    Discard {
        project: String,
        files: Vec<String>,
    },
    DeleteUntracked {
        project: String,
        files: Vec<String>,
    },
}

impl Op {
    /// Every op tag, in [`Op::name`] order.
    pub const NAMES: &'static [&'static str] = &[
        "subscribe",
        "unsubscribe",
        "outcome",
        "models",
        "diff",
        "attach",
        "detach",
        "ping",
        "new_session",
        "resume",
        "restart",
        "stop",
        "kill",
        "prompt",
        "interrupt",
        "switch_model",
        "set_effort",
        "action",
        "answer",
        "cancel",
        "inbox_open",
        "inbox_dismiss",
        "inbox_clear",
        "stage",
        "unstage",
        "delete_sessions",
        "copy_session",
        "move_session",
        "discard",
        "delete_untracked",
    ];

    pub fn name(&self) -> &'static str {
        match self {
            Op::Subscribe { .. } => "subscribe",
            Op::Unsubscribe { .. } => "unsubscribe",
            Op::Outcome { .. } => "outcome",
            Op::Models {} => "models",
            Op::Diff { .. } => "diff",
            Op::Attach { .. } => "attach",
            Op::Detach { .. } => "detach",
            Op::Ping {} => "ping",
            Op::NewSession { .. } => "new_session",
            Op::Resume { .. } => "resume",
            Op::Restart { .. } => "restart",
            Op::Stop { .. } => "stop",
            Op::Kill { .. } => "kill",
            Op::Prompt { .. } => "prompt",
            Op::Interrupt { .. } => "interrupt",
            Op::SwitchModel { .. } => "switch_model",
            Op::SetEffort { .. } => "set_effort",
            Op::Action { .. } => "action",
            Op::Answer { .. } => "answer",
            Op::Cancel { .. } => "cancel",
            Op::InboxOpen { .. } => "inbox_open",
            Op::InboxDismiss { .. } => "inbox_dismiss",
            Op::InboxClear {} => "inbox_clear",
            Op::Stage { .. } => "stage",
            Op::Unstage { .. } => "unstage",
            Op::DeleteSessions { .. } => "delete_sessions",
            Op::CopySession { .. } => "copy_session",
            Op::MoveSession { .. } => "move_session",
            Op::Discard { .. } => "discard",
            Op::DeleteUntracked { .. } => "delete_untracked",
        }
    }

    pub fn class(&self) -> Class {
        match self {
            Op::Subscribe { .. }
            | Op::Unsubscribe { .. }
            | Op::Outcome { .. }
            | Op::Models {}
            | Op::Diff { .. }
            | Op::Detach { .. }
            | Op::Ping {} => Class::View,
            Op::Attach { mode, .. } => match mode {
                ViewMode::Observe => Class::View,
                ViewMode::Input | ViewMode::Control => Class::Control,
            },
            Op::NewSession { .. }
            | Op::Resume { .. }
            | Op::Restart { .. }
            | Op::Stop { .. }
            | Op::Kill { .. }
            | Op::Prompt { .. }
            | Op::Interrupt { .. }
            | Op::SwitchModel { .. }
            | Op::SetEffort { .. }
            | Op::Action { .. }
            | Op::Answer { .. }
            | Op::Cancel { .. }
            | Op::InboxOpen { .. }
            | Op::InboxDismiss { .. }
            | Op::InboxClear {}
            | Op::Stage { .. }
            | Op::Unstage { .. } => Class::Control,
            Op::DeleteSessions { .. }
            | Op::CopySession { .. }
            | Op::MoveSession { .. }
            | Op::Discard { .. }
            | Op::DeleteUntracked { .. } => Class::Manage,
        }
    }

    /// Journaled (needs a `seq`): every Control/Manage op except `attach`/`detach`.
    pub fn journaled(&self) -> bool {
        match self {
            Op::Attach { .. } | Op::Detach { .. } => false,
            other => other.class() != Class::View,
        }
    }

    /// Allowed with `permissions`; `Err` names what is missing.
    pub fn allowed(&self, permissions: Permissions) -> Result<(), Class> {
        match self.class() {
            Class::View => Ok(()),
            Class::Control if permissions.control => Ok(()),
            Class::Manage if permissions.manage => Ok(()),
            need => Err(need),
        }
    }
}

// ---- feeds --------------------------------------------------------------------------------------

/// A subscription (tag `feed`); its values are [`Event::Feed`], latest only.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "feed", rename_all = "snake_case", deny_unknown_fields)]
pub enum Feed {
    /// Projects without their sessions (see [`Feed::Sessions`]).
    Projects {},
    /// One project's sessions, newest first: at most `limit` (1..=[`MAX_SESSIONS_LIMIT`]).
    Sessions {
        project: String,
        limit: u32,
    },
    Live {},
    Inbox {},
    /// At most [`MAX_CARDS`] session ids.
    Cards {
        sessions: Vec<String>,
    },
    /// The session's (or its subagent `agent`'s) transcript: the last `tail` (≤ [`MAX_TAIL`])
    /// items, then appends.
    Transcript {
        session: String,
        #[serde(default)]
        agent: Option<String>,
        tail: u32,
    },
    Changes {
        project: String,
    },
}

impl Feed {
    /// Every `feed` tag, in [`Feed::name`] order.
    pub const NAMES: &'static [&'static str] = &[
        "projects",
        "sessions",
        "live",
        "inbox",
        "cards",
        "transcript",
        "changes",
    ];

    pub fn name(&self) -> &'static str {
        match self {
            Feed::Projects {} => "projects",
            Feed::Sessions { .. } => "sessions",
            Feed::Live {} => "live",
            Feed::Inbox {} => "inbox",
            Feed::Cards { .. } => "cards",
            Feed::Transcript { .. } => "transcript",
            Feed::Changes { .. } => "changes",
        }
    }

    /// The parameter bounds (answered `bad_request` when broken).
    pub fn check(&self) -> Result<(), String> {
        match self {
            Feed::Sessions { limit, .. } if !(1..=MAX_SESSIONS_LIMIT).contains(limit) => {
                Err(format!("sessions limit must be 1..={MAX_SESSIONS_LIMIT}"))
            }
            Feed::Cards { sessions } if sessions.len() > MAX_CARDS => {
                Err(format!("at most {MAX_CARDS} cards"))
            }
            Feed::Transcript { tail, .. } if *tail > MAX_TAIL => {
                Err(format!("transcript tail must be at most {MAX_TAIL}"))
            }
            _ => Ok(()),
        }
    }
}

/// A feed's value (tag `feed`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "feed", rename_all = "snake_case")]
pub enum FeedValue {
    /// Most recently used first; `truncated` when older ones did not fit one message.
    Projects {
        projects: Vec<Project>,
        truncated: bool,
    },
    /// `total` sessions exist; `sessions` holds the newest (at most the feed's `limit`).
    Sessions {
        project: String,
        total: u32,
        sessions: Vec<SessionSummary>,
        truncated: bool,
    },
    Live {
        sessions: Vec<LiveSession>,
        truncated: bool,
    },
    Inbox {
        inbox: Inbox,
        truncated: bool,
    },
    Cards {
        cards: Vec<Card>,
        truncated: bool,
    },
    /// Items `start..start + items.len()` of the transcript; `reset` = forget earlier items
    /// (first value, or the file was rewritten). Large tails arrive as several values.
    Transcript {
        start: u64,
        items: Vec<Item>,
        reset: bool,
    },
    /// `changes` is `None` outside a git work tree; `truncated` when files were dropped.
    Changes {
        changes: Option<Changes>,
        error: Option<String>,
        truncated: bool,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Project {
    pub id: String,
    /// `~`-abbreviated folder.
    pub display: String,
    /// The folder is a linked git worktree.
    pub worktree: bool,
    pub session_count: u32,
    /// Its newest session's last modification.
    pub modified_unix_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSummary {
    /// omp's session id.
    pub id: String,
    pub title: Option<String>,
    /// At most [`FIRST_PROMPT_CHARS`] characters.
    pub first_prompt: Option<String>,
    pub modified_unix_ms: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LiveKind {
    Session,
    Tool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Activity {
    Unknown,
    Idle,
    Busy,
    Attention,
}

/// A session's indicator (tag `state`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Status {
    Idle {},
    Busy {},
    Attention {},
    Exited { clean: bool },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwitchView {
    pub selector: Option<String>,
    pub level: Option<String>,
}

/// A controlled session.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiveSession {
    pub live: LiveRef,
    pub kind: LiveKind,
    pub label: String,
    pub title: Option<String>,
    /// omp's session id when known.
    pub session: Option<String>,
    pub project: Option<String>,
    /// `~`-abbreviated folder.
    pub display: String,
    pub activity: Activity,
    pub status: Status,
    pub exit_code: Option<i32>,
    pub busy: bool,
    pub model: Option<String>,
    pub pending_switch: Option<SwitchView>,
    pub live_control: bool,
    /// A desktop view owns the geometry.
    pub desktop_controlled: bool,
    pub views: u32,
    /// This device's own requests, newest last.
    pub requests: Vec<OwnRequest>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnRequest {
    /// The controller's request id ([`Op::Answer::request`]).
    pub request: u64,
    pub seq: u64,
    pub label: String,
    pub delivery: DeliveryView,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Inbox {
    /// Newest first.
    pub entries: Vec<InboxItem>,
    pub warns: bool,
    pub badge: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InboxItem {
    pub live: LiveRef,
    pub kind: InboxKind,
    pub title: String,
    pub project: String,
    pub body: String,
    pub at_unix_ms: u64,
    pub armed: bool,
}

/// Tag `type`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InboxKind {
    Finished {},
    Error { message: Option<String> },
    Attention {},
    Exited { code: Option<i32> },
}

/// An agents-dashboard card.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Card {
    pub session: String,
    pub model: Option<String>,
    pub thinking: Option<String>,
    pub context_tokens: Option<u64>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Dollars; `None` when unknown or not finite.
    pub cost: Option<f64>,
    pub last_reply: Option<String>,
    pub last_turn_failed: bool,
    pub git: Option<GitCounts>,
    pub agents: AgentCounts,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitCounts {
    pub branch: String,
    pub ahead: u32,
    pub behind: u32,
    pub staged: u32,
    pub unstaged: u32,
    pub untracked: u32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentCounts {
    pub running: u32,
    pub done: u32,
    pub failed: u32,
    pub total: u32,
}

/// A transcript item (tag `type`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Item {
    User {
        text: String,
        from_agent: bool,
        at_unix_ms: Option<u64>,
    },
    Assistant {
        text: String,
        model: Option<String>,
        at_unix_ms: Option<u64>,
    },
    Thinking {
        text: String,
    },
    ToolCall {
        call_id: String,
        name: String,
        intent: Option<String>,
        summary: String,
        args: String,
    },
    ToolResult {
        call_id: String,
        name: String,
        text: String,
        is_error: bool,
        truncated: bool,
    },
    Notice {
        kind: String,
        text: String,
    },
    Irc {
        from: Option<String>,
        agent: Option<String>,
        text: String,
        note: Option<String>,
    },
    Compaction {
        tokens_before: Option<u64>,
        tokens_after: Option<u64>,
        summary: String,
    },
    ModelChange {
        model: String,
    },
    Error {
        message: String,
    },
    Exit {
        reason: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    Modified,
    Added,
    Deleted,
    Renamed,
    Copied,
    TypeChanged,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Changes {
    pub branch: Option<String>,
    pub files: Vec<ChangedFile>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangedFile {
    /// Repo-relative, `/`-separated (untracked directories end in `/`).
    pub path: String,
    pub orig_path: Option<String>,
    pub staged: Option<ChangeKind>,
    pub unstaged: Option<ChangeKind>,
    pub untracked: bool,
    pub conflicted: bool,
    pub is_dir: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diff {
    pub text: String,
    pub binary: bool,
    pub truncated: bool,
    pub added: u32,
    pub removed: u32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Model {
    pub selector: String,
    pub provider: String,
    pub id: String,
    pub name: String,
    pub context_window: Option<u64>,
    pub max_tokens: Option<u64>,
    pub reasoning: Option<bool>,
    /// Effort levels; empty = no thinking.
    pub thinking: Vec<String>,
    pub input: Vec<String>,
    /// omp's prices (`cost.input`/`cost.output`); `None` when unknown or not finite.
    pub cost_input: Option<f64>,
    pub cost_output: Option<f64>,
}

// ---- replies ------------------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Reply {
    pub id: u64,
    #[serde(flatten)]
    pub result: ReplyResult,
}

/// `{ "ok": … }` or `{ "err": … }` next to the id.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplyResult {
    Ok(Resp),
    Err(ApiError),
}

/// A successful reply (tag `type`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Resp {
    /// subscribe, unsubscribe, detach.
    Unit {},
    Pong {},
    /// A journaled op was admitted; its result follows as [`Event::Outcome`].
    Accepted {
        seq: u64,
        duplicate: bool,
    },
    Outcome {
        outcome: OutcomeView,
    },
    /// `truncated` when the catalogue did not fit one message.
    Models {
        models: Vec<Model>,
        truncated: bool,
    },
    Diff {
        diff: Diff,
    },
    /// Terminal records of `view` follow.
    Attached {
        view: u32,
        mode: ViewMode,
    },
}

/// A request's record (tag `state`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum OutcomeView {
    Running {},
    Done {
        result: DoneView,
    },
    Failed {
        message: String,
    },
    Rejected {
        reason: RejectedView,
    },
    Cancelled {},
    /// Accepted before the gateway stopped; it may or may not have run.
    Uncertain {},
    /// Finished under an earlier gateway instance.
    Recorded {
        succeeded: bool,
    },
    /// Never reached this gateway: the device may offer a user-initiated resend.
    Unknown {},
    /// Too old to be remembered.
    Expired {},
}

/// A final result (tag `type`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DoneView {
    Unit {},
    /// `new_session`, `resume`, `restart`.
    Session {
        live: LiveRef,
    },
    /// A command reached its final delivery.
    Submitted {
        request: u64,
        label: String,
        delivery: DeliveryView,
    },
    /// `answer`: `false` = no such outstanding confirmation.
    Answered {
        applied: bool,
    },
    /// `delete_sessions`: per session, `None` = deleted.
    Deleted {
        results: Vec<SessionResult>,
    },
    /// `copy_session`/`move_session`: the resulting session's id when it could be read.
    File {
        session: Option<String>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionResult {
    pub session: String,
    pub error: Option<String>,
}

/// Why the gateway did not run a command (tag `code`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "code", rename_all = "snake_case")]
pub enum RejectedView {
    Stale { current: Option<LiveRef> },
    Exited {},
    ReadOnly {},
    Detached {},
    Busy {},
    NoBracketedPaste {},
    NotInjected {},
    Empty {},
    NoChange {},
    ModelUnknown {},
    KeyReused {},
    Expired {},
    InputHeld {},
    BadPath {},
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Refusal {
    /// omp was not in the state the command needs.
    Precondition,
    DialogOpen,
    NoBracketedPaste,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CancelReason {
    Exited,
    Detached,
    Closing,
    SessionChanged,
    /// The device lost the permission the command needs (revoked or `control` lowered) before
    /// its first byte.
    NotAllowed,
}

/// How far a command got toward the terminal (tag `state`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum DeliveryView {
    Queued {},
    AwaitingConfirm { draft: String },
    Writing {},
    Delivered {},
    Refused { reason: Refusal },
    Declined {},
    Aborted {},
    Stalled {},
    Cancelled { reason: CancelReason },
    Unobserved {},
}

/// A refused request (tag `code`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "code", rename_all = "snake_case")]
pub enum ApiError {
    Unsupported {},
    NotAllowed { need: Class },
    BadRequest { message: String },
    NotFound { what: String },
    Stale { current: Option<LiveRef> },
    Rejected { reason: RejectedView },
    GeometryOwned {},
    TooManyRequests {},
    TooLarge {},
    Maintenance {},
    KeyReused {},
    Expired {},
    Failed { message: String },
}

// ---- events -------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputRefusal {
    /// The view may not type (observer, or `control` was lowered).
    NotAllowed,
    /// omp is receiving an injected command.
    InputHeld,
    Exited,
    ReadOnly,
    Detached,
    Stale,
    /// Another rejection of the session.
    Rejected,
}

/// Something the device reacts to (tag `type`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    Feed {
        feed: Feed,
        value: FeedValue,
    },
    Outcome {
        seq: u64,
        outcome: OutcomeView,
    },
    Progress {
        seq: u64,
        stage: String,
        done: u64,
        total: Option<u64>,
    },
    /// omp's composer holds `draft`: answer with [`Op::Answer`].
    Confirm {
        live: LiveRef,
        request: u64,
        label: String,
        draft: String,
    },
    TermState {
        view: u32,
        input_held: bool,
    },
    InputRefused {
        view: u32,
        reason: InputRefusal,
    },
    ModelUnconfirmed {
        live: LiveRef,
        selector: Option<String>,
        level: Option<String>,
    },
    /// Events were dropped: re-read the outcomes of open requests (feeds re-send themselves).
    Resync {},
}

/// Why a terminal view ended (`T_END`, tag `reason`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum TermEnd {
    Exited {
        code: Option<i32>,
    },
    Closed {},
    /// The session restarted: re-attach to its new generation.
    Stale {},
    /// The device fell behind: re-attach for a fresh snapshot.
    Overflow {},
    Disconnected {},
    Shutdown {},
    /// Detached by the device.
    Detached {},
    /// The device lost the permission the view needs.
    NotAllowed {},
    /// The snapshot exceeded [`MAX_SNAPSHOT`].
    TooLarge {},
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_round_trip_and_refuse_bad_shapes() {
        let records = vec![
            Record::Request(br#"{"id":1,"seq":null,"op":{"op":"ping"}}"#.to_vec()),
            Record::Ack { messages: 7 },
            Record::Input {
                view: 3,
                ctrl_c: true,
                bytes: vec![3],
            },
            Record::Resize {
                view: 3,
                cols: 80,
                rows: 24,
            },
        ];
        let bytes = encode(&records).unwrap();
        assert_eq!(decode(&bytes, Origin::Device).unwrap(), records);
        assert_eq!(
            decode(&bytes, Origin::Gateway),
            Err(RecordError::WrongDirection(kind::REQUEST))
        );
        assert_eq!(decode(&[], Origin::Device), Err(RecordError::Empty));
        assert_eq!(encode(&[]), Err(RecordError::Empty));
        // The kind partition: an unknown device-range kind is refused from a device, an
        // unknown gateway-range kind skipped from a gateway; the other side's range is the
        // wrong direction and reserved kinds are refused both ways.
        let one = |k: u8| [k, 0, 0, 0, 1, 9];
        assert_eq!(
            decode(&one(0x05), Origin::Device),
            Err(RecordError::UnknownKind(0x05))
        );
        assert_eq!(decode(&one(0x7f), Origin::Gateway).unwrap(), vec![]);
        assert_eq!(decode(&one(0x13), Origin::Gateway).unwrap(), vec![]);
        assert_eq!(
            decode(&one(0x7f), Origin::Device),
            Err(RecordError::WrongDirection(0x7f))
        );
        assert_eq!(
            decode(&one(0x0f), Origin::Gateway),
            Err(RecordError::WrongDirection(0x0f))
        );
        for reserved in [0x00, 0x80, 0xff] {
            for origin in [Origin::Device, Origin::Gateway] {
                assert_eq!(
                    decode(&one(reserved), origin),
                    Err(RecordError::UnknownKind(reserved)),
                    "{reserved:#04x} {origin:?}"
                );
            }
        }
        for k in [kind::REQUEST, kind::ACK, kind::INPUT, kind::RESIZE] {
            assert!(kind::DEVICE.contains(&k) && Record::device_kind(k));
        }
        for k in [
            kind::WELCOME,
            kind::REPLY,
            kind::EVENT,
            kind::T_SNAPSHOT,
            kind::T_OUTPUT,
            kind::T_RESIZE,
            kind::T_END,
        ] {
            assert!(kind::GATEWAY.contains(&k) && Record::gateway_kind(k));
        }
        // Truncated body, bad flags, bad ack length.
        assert_eq!(
            decode(&[kind::ACK, 0, 0, 0, 8, 1], Origin::Device),
            Err(RecordError::Truncated)
        );
        assert_eq!(
            decode(&[kind::INPUT, 0, 0, 0, 5, 0, 0, 0, 1, 2], Origin::Device),
            Err(RecordError::Malformed(kind::INPUT))
        );
        assert_eq!(
            decode(&[kind::ACK, 0, 0, 0, 1, 1], Origin::Device),
            Err(RecordError::Malformed(kind::ACK))
        );
        // Limits.
        let big = Record::Input {
            view: 1,
            ctrl_c: false,
            bytes: vec![0; MAX_INPUT + 1],
        };
        assert!(matches!(encode(&[big]), Err(RecordError::Oversized { .. })));
        let snap = Record::Snapshot {
            view: 1,
            last: true,
            bytes: vec![0; MAX_SNAPSHOT_CHUNK],
        };
        let one = encode(std::slice::from_ref(&snap)).unwrap();
        assert_eq!(decode(&one, Origin::Gateway).unwrap(), vec![snap]);
        let mut bad_last = one.clone();
        bad_last[RECORD_HEADER + 4] = 2;
        assert_eq!(
            decode(&bad_last, Origin::Gateway),
            Err(RecordError::Malformed(kind::T_SNAPSHOT))
        );
        let five = vec![
            Record::Output {
                view: 1,
                bytes: vec![0; 300_000],
            };
            4
        ];
        assert!(matches!(encode(&five), Err(RecordError::TooLarge(_))));
    }

    #[test]
    fn feed_parameters_are_bounded() {
        let sessions = |limit| Feed::Sessions {
            project: "p".into(),
            limit,
        };
        assert!(sessions(0).check().is_err());
        assert!(sessions(1).check().is_ok());
        assert!(sessions(MAX_SESSIONS_LIMIT).check().is_ok());
        assert!(sessions(MAX_SESSIONS_LIMIT + 1).check().is_err());
        let cards = |n| Feed::Cards {
            sessions: vec!["s".into(); n],
        };
        assert!(cards(MAX_CARDS).check().is_ok());
        assert!(cards(MAX_CARDS + 1).check().is_err());
        let tail = |tail| Feed::Transcript {
            session: "s".into(),
            agent: None,
            tail,
        };
        assert!(tail(MAX_TAIL).check().is_ok());
        assert!(tail(MAX_TAIL + 1).check().is_err());
    }

    #[test]
    fn class_table() {
        let live = LiveRef {
            key: "k".into(),
            generation: 1,
        };
        let attach = |mode| Op::Attach {
            live: live.clone(),
            mode,
        };
        assert_eq!(attach(ViewMode::Observe).class(), Class::View);
        assert_eq!(attach(ViewMode::Input).class(), Class::Control);
        assert!(!attach(ViewMode::Control).journaled());
        assert!(!Op::Detach { view: 1 }.journaled());
        assert!(Op::Cancel { seq: 1 }.journaled());
        assert_eq!(
            Op::DeleteSessions { sessions: vec![] }.class(),
            Class::Manage
        );
        let view_only = Permissions::default();
        assert_eq!(
            Op::Kill { live: live.clone() }.allowed(view_only),
            Err(Class::Control)
        );
        let control = Permissions {
            control: true,
            manage: false,
        };
        assert_eq!(Op::Kill { live }.allowed(control), Ok(()));
        assert_eq!(
            Op::Discard {
                project: "p".into(),
                files: vec![]
            }
            .allowed(control),
            Err(Class::Manage)
        );
        for (i, name) in Op::NAMES.iter().enumerate() {
            assert!(!Op::NAMES[..i].contains(name), "{name} twice");
        }
    }

    #[test]
    fn requests_parse_strictly_and_unknown_ops_are_answerable() {
        let ok = Request::parse(
            br#"{"id":4,"seq":9,"op":{"op":"interrupt","live":{"key":"k","generation":2}}}"#,
        )
        .unwrap();
        assert_eq!(ok.seq, Some(9));
        assert!(ok.check_seq().is_ok());
        // A missing optional is fine; an unknown field is not.
        let r = Request::parse(br#"{"id":4,"op":{"op":"ping"}}"#).unwrap();
        assert_eq!(r.seq, None);
        assert!(matches!(
            Request::parse(br#"{"id":4,"op":{"op":"ping","x":1}}"#),
            Err(RequestError::Bad { id: 4, .. })
        ));
        assert!(matches!(
            Request::parse(br#"{"id":4,"op":{"op":"ping"},"x":1}"#),
            Err(RequestError::Bad { id: 4, .. })
        ));
        assert_eq!(
            Request::parse(br#"{"id":5,"op":{"op":"teleport"}}"#),
            Err(RequestError::Unsupported { id: 5 })
        );
        // Unknown nested tags and enum values are unsupported too; a non-string is not.
        for (id, body) in [
            (
                6,
                r#"{"id":6,"op":{"op":"subscribe","feed":{"feed":"weather"}}}"#,
            ),
            (
                7,
                r#"{"id":7,"op":{"op":"unsubscribe","feed":{"feed":"weather"}}}"#,
            ),
            (
                8,
                r#"{"id":8,"op":{"op":"diff","project":"p","file":"f","side":"both"}}"#,
            ),
            (
                9,
                r#"{"id":9,"op":{"op":"attach","live":{"key":"k","generation":1},"mode":"x"}}"#,
            ),
            (
                10,
                r#"{"id":10,"seq":1,"op":{"op":"action","live":{"key":"k","generation":1},"action":{"type":"undo"}}}"#,
            ),
            (
                11,
                r#"{"id":11,"seq":1,"op":{"op":"action","live":{"key":"k","generation":1},"action":{"type":"compact","mode":"turbo"}}}"#,
            ),
        ] {
            assert_eq!(
                Request::parse(body.as_bytes()),
                Err(RequestError::Unsupported { id }),
                "{body}"
            );
        }
        for body in [
            r#"{"id":12,"op":{"op":"diff","project":"p","file":"f","side":1}}"#,
            r#"{"id":12,"op":{"op":"subscribe","feed":{}}}"#,
            r#"{"id":12,"seq":1,"op":{"op":"action","live":{"key":"k","generation":1},"action":{"type":"compact"}}}"#,
        ] {
            assert!(
                matches!(
                    Request::parse(body.as_bytes()),
                    Err(RequestError::Bad { id: 12, .. })
                ),
                "{body}"
            );
        }
        assert_eq!(
            Request::parse(br#"{"op":{"op":"ping"}}"#),
            Err(RequestError::Unreadable)
        );
        assert_eq!(Request::parse(b"[1]"), Err(RequestError::Unreadable));
        // seq rule.
        let r = Request::parse(br#"{"id":1,"op":{"op":"inbox_clear"}}"#).unwrap();
        assert!(r.check_seq().is_err());
        let r = Request::parse(br#"{"id":1,"seq":3,"op":{"op":"ping"}}"#).unwrap();
        assert!(r.check_seq().is_err());
    }

    #[test]
    fn project_ids_hash_the_lossy_utf8_path() {
        let id = project_id(b"/home/u/x");
        assert_eq!(id.len(), 18);
        assert!(id.starts_with("p-"));
        let digest = Sha256::digest(b"/home/u/x");
        let hex: String = digest[..8].iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(id, format!("p-{hex}"));
        assert_eq!(project_id(b"a\xffb"), project_id("a\u{fffd}b".as_bytes()));
    }
}
