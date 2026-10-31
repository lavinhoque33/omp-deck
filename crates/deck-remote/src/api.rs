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
//! **Slice-4 additions** (`docs/notes/mobile-pwa.md`, "Wire additions"): new ops, feeds and
//! types follow the same conventions. A device sends a new feed, optional request field, enum
//! value or record kind only when [`Welcome::features`] lists [`FEATURE_S4`]; the gateway sends
//! a new enum variant, `Event` or `Item` type only to a device that announced it. Fields added
//! to R5 structs after the freeze ([`Welcome::push_key`], [`SessionSummary::stub`],
//! [`LiveSession::pending_ui`], and `images`, `args_value` and `details` of [`Item`]) are the
//! one exception to "`None` is written as `null`": they are omitted when empty, absent or
//! `false` (`#[serde(default, skip_serializing_if = …)]`), so every R5 message still encodes
//! byte-identically (`tests/vectors/api-v1.json`), and a reader takes a missing one as empty.
//! The slice-4 vectors are `tests/vectors/api-s4.json`.
//!
//! A request that is well-formed JSON with a numeric `id` but names an op, feed, session
//! action type, tagged request value or enum value this gateway does not know (for example
//! [`ViewMode`], [`DiffSide`], [`CompactMode`], [`ComposeMode`], [`UiAnswer`]) is answered
//! [`ApiError::Unsupported`], any other bad request [`ApiError::BadRequest`]
//! ([`Request::parse`]): a newer device probing an older gateway can tell the two apart.
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
//! Devices never see absolute paths, ctl keys, mux names, other clients' request keys or
//! secrets: projects are [`project_id`]s, sessions omp session ids, live sessions [`LiveRef`]s,
//! changed files repo-relative paths, folders opaque dir tokens (`"f-"` + 16 hex, at most
//! [`MAX_DIR_TOKENS`] per device) and running mux sessions opaque running ids (`"r-"` + 16
//! hex); folders are shown as `~`-abbreviated displays, which are never accepted back.

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
/// - Slice 4: the `running`, `agents`, `worktrees`, `sync`, `usage` values and the `tree`,
///   `dir`, `omp_settings`, `backup_list`, `sync_backups` replies and `restore_plan` results
///   drop whole entries from the end with `truncated: true` (`worktrees`: parked entries,
///   then branches; `usage`: limits of the last report, then reports; `omp_settings`: entries
///   of the last group, then groups; `restore_plan`: list entries); `inventory` is at most
///   `limit` entries (`total` tells how many exist), then the same rule; `tree` pages by
///   `next_cursor`; `text` replies are cut to [`MAX_LAST_REPLY`] bytes with `truncated: true`;
///   [`LiveSession::pending_ui`] holds at most [`MAX_PENDING_UI`] requests, each string cut
///   to [`MAX_UI_TEXT`] bytes and each option list to [`MAX_UI_OPTIONS`] entries.
pub const MAX_TRANSCRIPT_BYTES: usize = 512 * 1024;
/// Bytes of one transcript item's text field at most (then [`CUT_MARK`]).
pub const MAX_ITEM_TEXT: usize = 64 * 1024;
/// Ends a text the gateway cut.
pub const CUT_MARK: &str = "…";
/// Bytes of a diff text at most.
pub const MAX_DIFF_TEXT: usize = 768 * 1024;

// ---- slice 4 bounds -----------------------------------------------------------------------------

/// The slice-4 feature name ([`Welcome::features`], `DeviceHello.features`).
pub const FEATURE_S4: &str = "s4";
/// [`Op::LastReply`]: bytes of the reply text at most, then `truncated`.
pub const MAX_LAST_REPLY: usize = 256 * 1024;
/// Raw bytes of one [`Resp::Chunk`] / [`Op::UploadChunk`] at most (sent as base64).
pub const MAX_TRANSFER_CHUNK: usize = 512 * 1024;
/// Base64 characters of one [`Op::UploadChunk`] `data` at most.
pub const MAX_TRANSFER_CHUNK_B64: usize = MAX_TRANSFER_CHUNK.div_ceil(3) * 4;
/// Open download handles per device.
pub const MAX_DOWNLOADS: usize = 8;
/// Open uploads per device.
pub const MAX_UPLOADS: usize = 4;
/// A download or upload unused for this long expires.
pub const TRANSFER_IDLE_SECS: u64 = 600;
/// Bytes of one uploaded file at most.
pub const MAX_UPLOAD_FILE: u64 = 20 << 20;
/// Bytes of a device's attachments at most.
pub const MAX_UPLOAD_TOTAL: u64 = 200 << 20;
/// [`Op::Tree`] `limit` range: `1..=MAX_TREE_LIMIT`.
pub const MAX_TREE_LIMIT: u32 = 2000;
/// [`Op::UsageActivity`] `days` range: `1..=MAX_ACTIVITY_DAYS`.
pub const MAX_ACTIVITY_DAYS: u32 = 371;
/// [`Feed::Inventory`] `limit` range: `1..=MAX_INVENTORY_LIMIT`.
pub const MAX_INVENTORY_LIMIT: u32 = 500;
/// [`Item::ToolCall::args_value`]: each string cut to this many bytes (then [`CUT_MARK`]) …
pub const MAX_ARGS_STRING: usize = 16 * 1024;
/// … and the whole value at most this many bytes serialized, else absent.
pub const MAX_ARGS_VALUE: usize = 64 * 1024;
/// [`Item::ToolResult::details`] at most this many bytes serialized, else absent.
pub const MAX_DETAILS: usize = 32 * 1024;
/// [`LiveSession::pending_ui`] entries at most.
pub const MAX_PENDING_UI: usize = 8;
/// Bytes of each string of a [`UiRequestView`] at most (then [`CUT_MARK`]).
pub const MAX_UI_TEXT: usize = 4 * 1024;
/// Entries of each option list of a [`UiRequestView`] at most.
pub const MAX_UI_OPTIONS: usize = 50;
/// Dir tokens a device holds at most (its oldest are forgotten first).
pub const MAX_DIR_TOKENS: usize = 4096;
/// [`TreeNode::text`] characters at most.
pub const TREE_TEXT_CHARS: usize = 200;

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
    /// Slice 4 (post-R5, omitted when empty): the Web Push VAPID public key, base64url of the
    /// 65-byte uncompressed P-256 point. A device resubscribes when it changes.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub push_key: String,
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

/// The op names a tag or string enum value outside this version's sets: every place an [`Op`]
/// holds a tag or a plain enum (keep in step when one is added; `tests/api.rs` checks it
/// against serde).
fn names_unknown(op: &serde_json::Value) -> bool {
    use serde_json::Value;
    let unknown = |v: Option<&Value>, names: &[&str]| matches!(v, Some(Value::String(s)) if !names.contains(&s.as_str()));
    // Any element of the list `v` whose `field` is unknown.
    let each = |v: Option<&Value>, field: &str, names: &[&str]| {
        v.and_then(Value::as_array)
            .is_some_and(|a| a.iter().any(|e| unknown(e.get(field), names)))
    };
    let Some(Value::String(name)) = op.get("op") else {
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
                || (kind.and_then(Value::as_str) == Some("compact")
                    && unknown(action.and_then(|a| a.get("mode")), CompactMode::NAMES))
        }
        "tree" => unknown(op.get("filter"), TreeFilter::NAMES),
        "compose" => unknown(op.get("mode"), ComposeMode::NAMES),
        "review" => each(op.get("comments"), "side", DiffSide::NAMES),
        "answer_ui" => {
            let value = op.get("value");
            let kind = value.and_then(|v| v.get("type"));
            unknown(kind, UiAnswer::NAMES)
                || (kind.and_then(Value::as_str) == Some("approve")
                    && unknown(
                        value
                            .and_then(|v| v.get("decision"))
                            .and_then(|d| d.get("type")),
                        ApproveDecision::NAMES,
                    ))
        }
        "worktree_continue" => unknown(op.get("where"), ContinueWhere::NAMES),
        "open_on_desktop" => unknown(op.get("split"), Split::NAMES),
        "update_run" => unknown(op.get("kind"), UpdateKind::NAMES),
        "app_update" => unknown(op.get("step"), AppUpdateStep::NAMES),
        "sync_restore_prepare" => unknown(
            op.get("target").and_then(|t| t.get("type")),
            RestoreTarget::NAMES,
        ),
        "sync_restore_apply" => each(op.get("conflicts"), "choice", ConflictResolution::NAMES),
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

// ---- slice 4: request types (device → gateway, strict) ------------------------------------------

/// How [`Op::Compose`] delivers its text.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ComposeMode {
    /// Submit (idle only).
    Send,
    /// Paste and Enter while omp is busy (omp steers).
    Steer,
    /// Paste and Ctrl+Q while omp is busy (omp's queued follow-up).
    FollowUp,
    /// Paste without submitting (any state).
    Insert,
}

impl ComposeMode {
    /// Every value.
    pub const NAMES: &'static [&'static str] = &["send", "steer", "follow_up", "insert"];
}

/// One review comment of [`Op::Review`]: lines `start..=end` of a repo-relative changed file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewComment {
    pub file: String,
    pub side: DiffSide,
    pub start: u32,
    pub end: u32,
    pub text: String,
}

/// Which nodes [`Op::Tree`] lists (omp's tree filters).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TreeFilter {
    Conversation,
    Default,
    All,
    Labeled,
}

impl TreeFilter {
    /// Every value.
    pub const NAMES: &'static [&'static str] = &["conversation", "default", "all", "labeled"];
}

/// Where [`Op::OpenOnDesktop`] opens the session's tab.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Split {
    None,
    Right,
    Down,
}

impl Split {
    /// Every value.
    pub const NAMES: &'static [&'static str] = &["none", "right", "down"];
}

/// Where [`Op::WorktreeContinue`] continues a parked branch.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContinueWhere {
    /// In a new worktree of the branch.
    Worktree,
    /// In the main checkout.
    Main,
    /// In the main checkout, then back to its previous branch when done.
    SwitchBack,
}

impl ContinueWhere {
    /// Every value.
    pub const NAMES: &'static [&'static str] = &["worktree", "main", "switch_back"];
}

/// What [`Op::UpdateRun`] runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpdateKind {
    Omp,
    Plugins,
    Stable,
    Canary,
}

impl UpdateKind {
    /// Every value.
    pub const NAMES: &'static [&'static str] = &["omp", "plugins", "stable", "canary"];
}

/// [`Op::AppUpdate`]: OmpDeck's own update. `apply` is refused for installs that need the
/// desktop (package installs, [`AppUpdateView::apply_allowed`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AppUpdateStep {
    Download,
    Apply,
}

impl AppUpdateStep {
    /// Every value.
    pub const NAMES: &'static [&'static str] = &["download", "apply"];
}

/// Which parts of a project are synced or restored.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SyncParts {
    /// The project's `.omp` directory.
    pub omp: bool,
    pub sessions: bool,
    /// The memory bank.
    pub memory: bool,
}

/// Where [`Op::SyncRestorePrepare`] restores to (tag `type`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum RestoreTarget {
    /// A known project.
    Project { project: String },
    /// The folder of a dir token.
    Dir { dir: String },
}

impl RestoreTarget {
    /// Every `type` tag.
    pub const NAMES: &'static [&'static str] = &["project", "dir"];
}

/// What a project restore does with a backed-up session whose id exists locally.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictResolution {
    /// Leave the local session.
    Keep,
    /// Replace the local copies.
    Overwrite,
    /// Restore the backup under a fresh id next to the local session.
    Both,
}

impl ConflictResolution {
    /// Every value.
    pub const NAMES: &'static [&'static str] = &["keep", "overwrite", "both"];
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConflictChoice {
    pub session: String,
    pub choice: ConflictResolution,
}

/// Which events this device is pushed, and whether a push names the session.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PushPrefs {
    pub turn_finished: bool,
    pub attention: bool,
    pub ended: bool,
    /// Include the session title (cut to 80 characters) instead of a generic text.
    pub show_titles: bool,
}

impl Default for PushPrefs {
    fn default() -> PushPrefs {
        PushPrefs {
            turn_finished: true,
            attention: true,
            ended: true,
            show_titles: false,
        }
    }
}

/// An answer to a [`UiRequestView`] (tag `type`), checked against its kind and bounds.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum UiAnswer {
    /// `select` (single): the option index.
    Choice { index: u32 },
    /// `select` (`multi`): the option indices.
    Choices { indices: Vec<u32> },
    /// `input` / `editor`.
    Text { text: String },
    /// `confirm`.
    Confirm { yes: bool },
    /// `ask`: one answer per question.
    Ask { answers: Vec<UiAskAnswer> },
    /// `approve`.
    Approve { decision: ApproveDecision },
    /// Any kind: dismiss it (omp sees a cancel).
    Cancel {},
}

impl UiAnswer {
    /// Every `type` tag.
    pub const NAMES: &'static [&'static str] = &[
        "choice", "choices", "text", "confirm", "ask", "approve", "cancel",
    ];
}

/// The answer to one [`UiQuestion`]: the selected option indices and an optional own text.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiAskAnswer {
    pub id: String,
    pub selected: Vec<u32>,
    #[serde(default)]
    pub custom: Option<String>,
}

/// A tool-approval decision (tag `type`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ApproveDecision {
    Once {},
    /// For the rest of the session.
    Session {},
    Deny {},
    /// The [`ApproveView::grants`] entry `index`; `always` = its always-variant.
    Grant {
        index: u32,
        always: bool,
    },
}

impl ApproveDecision {
    /// Every `type` tag.
    pub const NAMES: &'static [&'static str] = &["once", "session", "deny", "grant"];
}

/// Operations (tag `op`): v1 and the slice-4 additions.
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
    // ---- slice 4: View ----
    /// The session's last assistant reply: [`Resp::Text`] (at most [`MAX_LAST_REPLY`] bytes).
    LastReply {
        session: String,
    },
    /// omp `--export` HTML of the session: [`Resp::Download`].
    Export {
        session: String,
    },
    /// An image an [`ImageRef`] of the session (or its subagent `agent`) names:
    /// [`Resp::Download`].
    Blob {
        session: String,
        #[serde(default)]
        agent: Option<String>,
        blob: String,
    },
    /// The next chunk of a download handle from `offset`: [`Resp::Chunk`].
    Download {
        download: String,
        offset: u64,
    },
    /// One page of the session tree: [`Resp::Tree`]; `limit` `1..=`[`MAX_TREE_LIMIT`].
    Tree {
        session: String,
        filter: TreeFilter,
        #[serde(default)]
        cursor: Option<String>,
        limit: u32,
    },
    /// The sub-folders of dir token `dir` (`None`: the roots): [`Resp::Dir`].
    Browse {
        #[serde(default)]
        dir: Option<String>,
    },
    /// Per-day usage of the last `days` (`1..=`[`MAX_ACTIVITY_DAYS`]): [`Resp::Activity`].
    UsageActivity {
        days: u32,
    },
    UsageRefresh {},
    /// [`Resp::OmpSettings`].
    OmpSettings {},
    /// [`Resp::BackupList`]: the active destination's config backups.
    BackupList {},
    /// [`Resp::SyncBackups`] of `project` (`None`: all).
    SyncBackups {
        #[serde(default)]
        project: Option<String>,
    },
    /// Push is skipped while the device reports itself visible on a live connection.
    Presence {
        visible: bool,
    },
    /// A Web Push subscription (`p256dh`, `auth` base64url).
    PushSubscribe {
        endpoint: String,
        p256dh: String,
        auth: String,
    },
    PushUnsubscribe {},
    PushPrefs {
        prefs: PushPrefs,
    },
    /// Renames this device (display-safe, as at pairing).
    DeviceRename {
        name: String,
    },
    /// Answered, then this device is revoked.
    Unpair {},
    // ---- slice 4: Control ----
    /// [`Resp::Upload`]; at most [`MAX_UPLOADS`] open, [`MAX_UPLOAD_FILE`] bytes.
    UploadBegin {
        name: String,
        mime: String,
        size: u64,
    },
    /// `data`: base64 of at most [`MAX_TRANSFER_CHUNK`] bytes written at `offset`.
    UploadChunk {
        upload: String,
        offset: u64,
        data: String,
    },
    UploadCancel {
        upload: String,
    },
    /// Pastes the finished uploads `attachments` (as paths), then `text`, then submits as
    /// `mode` says.
    Compose {
        live: LiveRef,
        text: String,
        attachments: Vec<String>,
        mode: ComposeMode,
    },
    /// Review comments as one prompt, inserted (`send: false`) or sent.
    Review {
        live: LiveRef,
        comments: Vec<ReviewComment>,
        #[serde(default)]
        general: Option<String>,
        send: bool,
    },
    /// Answers [`UiRequestView::request`]: [`DoneView::Answered`] (`false`: it is gone).
    AnswerUi {
        live: LiveRef,
        request: String,
        value: UiAnswer,
    },
    AgentKill {
        live: LiveRef,
        agent: String,
    },
    AgentRevive {
        live: LiveRef,
        agent: String,
    },
    AgentChat {
        live: LiveRef,
        agent: String,
        text: String,
    },
    /// Branches the session at `entry` into a new file: [`DoneView::File`].
    ForkAt {
        session: String,
        entry: String,
    },
    /// Moves the live session's leaf to `entry` (idle only).
    Navigate {
        live: LiveRef,
        entry: String,
    },
    /// A new session in the folder of a dir token: [`DoneView::Session`].
    NewSessionIn {
        dir: String,
    },
    /// Kills running mux sessions by running id: [`DoneView::Killed`].
    RunningKill {
        ids: Vec<String>,
    },
    /// [`DoneView::Session`] when `open`, else [`DoneView::Unit`].
    WorktreeCreate {
        project: String,
        branch: String,
        #[serde(default)]
        base: Option<String>,
        open: bool,
    },
    WorktreeMove {
        live: LiveRef,
        branch: String,
    },
    /// Runs the finish flow of the worktree `project` on the gateway.
    WorktreeFinish {
        project: String,
    },
    /// Continues a parked branch ([`ParkedView::id`]): [`DoneView::Session`].
    WorktreeContinue {
        parked: String,
        r#where: ContinueWhere,
    },
    /// Not journaled.
    UpdateCheck {},
    /// Backs up the omp config to the active destination now.
    BackupRun {},
    SyncBackup {
        project: String,
    },
    OpenOnDesktop {
        session: String,
        split: Split,
    },
    CloseOnDesktop {
        session: String,
    },
    // ---- slice 4: Manage ----
    /// Removes the worktree `project`; `park` keeps its branch as a [`ParkedView`].
    WorktreeRemove {
        project: String,
        park: bool,
    },
    OmpSet {
        key: String,
        value: serde_json::Value,
    },
    OmpReset {
        key: String,
    },
    /// Runs in a session the device can attach to: [`DoneView::Session`].
    UpdateRun {
        kind: UpdateKind,
    },
    AppUpdate {
        step: AppUpdateStep,
    },
    /// Sets up or signs in to the active destination on the desktop (Google sign-in in the
    /// desktop browser); refused `needs desktop` without a desktop session and for a backup
    /// server, which is set up in the desktop app.
    BackupConnect {},
    /// Deletes config backups of the active destination.
    BackupDelete {
        ids: Vec<String>,
    },
    /// [`DoneView::RestorePlan`].
    BackupRestorePrepare {
        id: String,
    },
    BackupRestoreApply {
        handle: u64,
    },
    BackupRestoreDiscard {
        handle: u64,
    },
    /// Adds the folder of a dir token to the sync rows.
    SyncAdd {
        dir: String,
    },
    /// `excluded`: session ids opted out.
    SyncSet {
        project: String,
        parts: SyncParts,
        excluded: Vec<String>,
    },
    SyncRemove {
        project: String,
    },
    SyncDelete {
        ids: Vec<String>,
    },
    /// [`DoneView::RestorePlan`].
    SyncRestorePrepare {
        backup: String,
        target: RestoreTarget,
        parts: SyncParts,
    },
    /// `excluded`: backed-up session ids ([`SessionPlanView::id`]) left out of the restore.
    SyncRestoreApply {
        handle: u64,
        conflicts: Vec<ConflictChoice>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        excluded: Vec<String>,
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
        "last_reply",
        "export",
        "blob",
        "download",
        "tree",
        "browse",
        "usage_activity",
        "usage_refresh",
        "omp_settings",
        "backup_list",
        "sync_backups",
        "presence",
        "push_subscribe",
        "push_unsubscribe",
        "push_prefs",
        "device_rename",
        "unpair",
        "upload_begin",
        "upload_chunk",
        "upload_cancel",
        "compose",
        "review",
        "answer_ui",
        "agent_kill",
        "agent_revive",
        "agent_chat",
        "fork_at",
        "navigate",
        "new_session_in",
        "running_kill",
        "worktree_create",
        "worktree_move",
        "worktree_finish",
        "worktree_continue",
        "update_check",
        "backup_run",
        "sync_backup",
        "open_on_desktop",
        "close_on_desktop",
        "worktree_remove",
        "omp_set",
        "omp_reset",
        "update_run",
        "app_update",
        "backup_connect",
        "backup_delete",
        "backup_restore_prepare",
        "backup_restore_apply",
        "backup_restore_discard",
        "sync_add",
        "sync_set",
        "sync_remove",
        "sync_delete",
        "sync_restore_prepare",
        "sync_restore_apply",
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
            Op::LastReply { .. } => "last_reply",
            Op::Export { .. } => "export",
            Op::Blob { .. } => "blob",
            Op::Download { .. } => "download",
            Op::Tree { .. } => "tree",
            Op::Browse { .. } => "browse",
            Op::UsageActivity { .. } => "usage_activity",
            Op::UsageRefresh {} => "usage_refresh",
            Op::OmpSettings {} => "omp_settings",
            Op::BackupList {} => "backup_list",
            Op::SyncBackups { .. } => "sync_backups",
            Op::Presence { .. } => "presence",
            Op::PushSubscribe { .. } => "push_subscribe",
            Op::PushUnsubscribe {} => "push_unsubscribe",
            Op::PushPrefs { .. } => "push_prefs",
            Op::DeviceRename { .. } => "device_rename",
            Op::Unpair {} => "unpair",
            Op::UploadBegin { .. } => "upload_begin",
            Op::UploadChunk { .. } => "upload_chunk",
            Op::UploadCancel { .. } => "upload_cancel",
            Op::Compose { .. } => "compose",
            Op::Review { .. } => "review",
            Op::AnswerUi { .. } => "answer_ui",
            Op::AgentKill { .. } => "agent_kill",
            Op::AgentRevive { .. } => "agent_revive",
            Op::AgentChat { .. } => "agent_chat",
            Op::ForkAt { .. } => "fork_at",
            Op::Navigate { .. } => "navigate",
            Op::NewSessionIn { .. } => "new_session_in",
            Op::RunningKill { .. } => "running_kill",
            Op::WorktreeCreate { .. } => "worktree_create",
            Op::WorktreeMove { .. } => "worktree_move",
            Op::WorktreeFinish { .. } => "worktree_finish",
            Op::WorktreeContinue { .. } => "worktree_continue",
            Op::UpdateCheck {} => "update_check",
            Op::BackupRun {} => "backup_run",
            Op::SyncBackup { .. } => "sync_backup",
            Op::OpenOnDesktop { .. } => "open_on_desktop",
            Op::CloseOnDesktop { .. } => "close_on_desktop",
            Op::WorktreeRemove { .. } => "worktree_remove",
            Op::OmpSet { .. } => "omp_set",
            Op::OmpReset { .. } => "omp_reset",
            Op::UpdateRun { .. } => "update_run",
            Op::AppUpdate { .. } => "app_update",
            Op::BackupConnect {} => "backup_connect",
            Op::BackupDelete { .. } => "backup_delete",
            Op::BackupRestorePrepare { .. } => "backup_restore_prepare",
            Op::BackupRestoreApply { .. } => "backup_restore_apply",
            Op::BackupRestoreDiscard { .. } => "backup_restore_discard",
            Op::SyncAdd { .. } => "sync_add",
            Op::SyncSet { .. } => "sync_set",
            Op::SyncRemove { .. } => "sync_remove",
            Op::SyncDelete { .. } => "sync_delete",
            Op::SyncRestorePrepare { .. } => "sync_restore_prepare",
            Op::SyncRestoreApply { .. } => "sync_restore_apply",
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
            Op::LastReply { .. }
            | Op::Export { .. }
            | Op::Blob { .. }
            | Op::Download { .. }
            | Op::Tree { .. }
            | Op::Browse { .. }
            | Op::UsageActivity { .. }
            | Op::UsageRefresh {}
            | Op::OmpSettings {}
            | Op::BackupList {}
            | Op::SyncBackups { .. }
            | Op::Presence { .. }
            | Op::PushSubscribe { .. }
            | Op::PushUnsubscribe {}
            | Op::PushPrefs { .. }
            | Op::DeviceRename { .. }
            | Op::Unpair {} => Class::View,
            Op::UploadBegin { .. }
            | Op::UploadChunk { .. }
            | Op::UploadCancel { .. }
            | Op::Compose { .. }
            | Op::Review { .. }
            | Op::AnswerUi { .. }
            | Op::AgentKill { .. }
            | Op::AgentRevive { .. }
            | Op::AgentChat { .. }
            | Op::ForkAt { .. }
            | Op::Navigate { .. }
            | Op::NewSessionIn { .. }
            | Op::RunningKill { .. }
            | Op::WorktreeCreate { .. }
            | Op::WorktreeMove { .. }
            | Op::WorktreeFinish { .. }
            | Op::WorktreeContinue { .. }
            | Op::UpdateCheck {}
            | Op::BackupRun {}
            | Op::SyncBackup { .. }
            | Op::OpenOnDesktop { .. }
            | Op::CloseOnDesktop { .. } => Class::Control,
            Op::DeleteSessions { .. }
            | Op::CopySession { .. }
            | Op::MoveSession { .. }
            | Op::Discard { .. }
            | Op::DeleteUntracked { .. }
            | Op::WorktreeRemove { .. }
            | Op::OmpSet { .. }
            | Op::OmpReset { .. }
            | Op::UpdateRun { .. }
            | Op::AppUpdate { .. }
            | Op::BackupConnect {}
            | Op::BackupDelete { .. }
            | Op::BackupRestorePrepare { .. }
            | Op::BackupRestoreApply { .. }
            | Op::BackupRestoreDiscard { .. }
            | Op::SyncAdd { .. }
            | Op::SyncSet { .. }
            | Op::SyncRemove { .. }
            | Op::SyncDelete { .. }
            | Op::SyncRestorePrepare { .. }
            | Op::SyncRestoreApply { .. } => Class::Manage,
        }
    }

    /// Journaled (needs a `seq`): every Control/Manage op except `attach`/`detach`, the
    /// uploads and `update_check`.
    pub fn journaled(&self) -> bool {
        match self {
            Op::Attach { .. }
            | Op::Detach { .. }
            | Op::UploadBegin { .. }
            | Op::UploadChunk { .. }
            | Op::UploadCancel { .. }
            | Op::UpdateCheck {} => false,
            other => other.class() != Class::View,
        }
    }

    /// The parameter bounds (answered `bad_request` when broken): [`Feed::check`] of a
    /// subscription, [`Op::Tree`] `limit` `1..=`[`MAX_TREE_LIMIT`], [`Op::UsageActivity`]
    /// `days` `1..=`[`MAX_ACTIVITY_DAYS`] and [`Op::UploadChunk`] `data` at most
    /// [`MAX_TRANSFER_CHUNK_B64`] characters.
    pub fn check(&self) -> Result<(), String> {
        match self {
            Op::Subscribe { feed } | Op::Unsubscribe { feed } => feed.check(),
            Op::Tree { limit, .. } if !(1..=MAX_TREE_LIMIT).contains(limit) => {
                Err(format!("tree limit must be 1..={MAX_TREE_LIMIT}"))
            }
            Op::UsageActivity { days } if !(1..=MAX_ACTIVITY_DAYS).contains(days) => {
                Err(format!("activity days must be 1..={MAX_ACTIVITY_DAYS}"))
            }
            Op::UploadChunk { data, .. } if data.len() > MAX_TRANSFER_CHUNK_B64 => Err(format!(
                "an upload chunk carries at most {MAX_TRANSFER_CHUNK} bytes"
            )),
            _ => Ok(()),
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
    // ---- slice 4 (gated by [`FEATURE_S4`]) ----
    /// Running mux sessions with memory ([`RunningView`]).
    Running {},
    /// Session files of `project` (`None`: every project), stubs included, newest first: at
    /// most `limit` (1..=[`MAX_INVENTORY_LIMIT`]).
    Inventory {
        #[serde(default)]
        project: Option<String>,
        limit: u32,
    },
    /// The live session's `/context` breakdown.
    Context {
        live: LiveRef,
    },
    /// Provider quotas.
    Usage {},
    /// The subagents of a session.
    Agents {
        session: String,
    },
    /// The repository of `project`: its worktree state and parked branches.
    Worktrees {
        project: String,
    },
    Backup {},
    Sync {},
    Updates {},
    Device {},
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
        "running",
        "inventory",
        "context",
        "usage",
        "agents",
        "worktrees",
        "backup",
        "sync",
        "updates",
        "device",
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
            Feed::Running {} => "running",
            Feed::Inventory { .. } => "inventory",
            Feed::Context { .. } => "context",
            Feed::Usage {} => "usage",
            Feed::Agents { .. } => "agents",
            Feed::Worktrees { .. } => "worktrees",
            Feed::Backup {} => "backup",
            Feed::Sync {} => "sync",
            Feed::Updates {} => "updates",
            Feed::Device {} => "device",
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
            Feed::Inventory { limit, .. } if !(1..=MAX_INVENTORY_LIMIT).contains(limit) => {
                Err(format!("inventory limit must be 1..={MAX_INVENTORY_LIMIT}"))
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
    // ---- slice 4 ----
    /// Running mux sessions, newest first; `server_bytes`: the tmux server's own memory.
    Running {
        sessions: Vec<RunningView>,
        server_bytes: Option<u64>,
        truncated: bool,
    },
    /// `total` session files exist (stubs included); `entries` holds the newest.
    Inventory {
        project: Option<String>,
        total: u32,
        entries: Vec<InventoryView>,
        truncated: bool,
    },
    /// `context` is `None` without live control or before omp reported one.
    Context {
        live: LiveRef,
        context: Option<ContextView>,
    },
    Usage {
        usage: Option<UsageView>,
        fetched_unix_ms: Option<u64>,
        error: Option<String>,
        /// A fetch is running ([`Op::UsageRefresh`]).
        fetching: bool,
        truncated: bool,
    },
    /// The session's subagents, parent before child, newest first among siblings.
    Agents {
        session: String,
        agents: Vec<AgentView>,
        truncated: bool,
    },
    /// `worktree` is `None` outside a git repository; `parked` lists the repository's parked
    /// branches.
    Worktrees {
        project: String,
        worktree: Option<WorktreeView>,
        parked: Vec<ParkedView>,
        truncated: bool,
    },
    /// Config backup to the active destination; `available`: the destination can be used in
    /// this build (Google credentials are configured; always true for a server), `connected`:
    /// it is set up (signed in / a server saved).
    Backup {
        destination: BackupDestinationView,
        available: bool,
        connected: bool,
        /// The Google account's email; absent for a backup server.
        account: Option<String>,
        last_backup_unix_ms: Option<u64>,
        interval: SyncInterval,
        /// The running backup job's label.
        busy: Option<String>,
        error: Option<String>,
    },
    /// Project sync rows.
    Sync {
        rows: Vec<SyncRowView>,
        interval: SyncInterval,
        busy: Option<String>,
        truncated: bool,
    },
    Updates {
        omp: OmpUpdateView,
        app: AppUpdateView,
    },
    /// This device as the gateway sees it.
    Device {
        name: String,
        permissions: Permissions,
        push: PushStateView,
        gateway_version: String,
        mode: GatewayMode,
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
    /// Slice 4 (post-R5, omitted when `false`): no title and no user prompt (an empty session
    /// omp left behind).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub stub: bool,
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
    /// Slice 4 (post-R5, omitted when empty): omp's pending structured UI requests, oldest
    /// first, at most [`MAX_PENDING_UI`]; answered with [`Op::AnswerUi`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending_ui: Vec<UiRequestView>,
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
        /// Slice 4 (post-R5, omitted when empty): attached images, fetched with [`Op::Blob`].
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<ImageRef>,
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
        /// Slice 4 (post-R5, omitted when absent): the structured arguments, each string cut
        /// to [`MAX_ARGS_STRING`] bytes (then [`CUT_MARK`]); absent when still over
        /// [`MAX_ARGS_VALUE`] bytes serialized.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        args_value: Option<serde_json::Value>,
    },
    ToolResult {
        call_id: String,
        name: String,
        text: String,
        is_error: bool,
        truncated: bool,
        /// Slice 4 (post-R5, omitted when absent): omp's tool-result `details`, absent when
        /// over [`MAX_DETAILS`] bytes serialized.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        details: Option<serde_json::Value>,
        /// Slice 4 (post-R5, omitted when empty): images of the result, fetched with
        /// [`Op::Blob`].
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<ImageRef>,
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
    // ---- slice 4 ----
    /// `last_reply`: cut to [`MAX_LAST_REPLY`] bytes with `truncated`.
    Text {
        text: String,
        truncated: bool,
    },
    /// `export`/`blob`: pull it with [`Op::Download`]; `size` in bytes.
    Download {
        download: String,
        name: String,
        mime: String,
        size: u64,
    },
    /// At most [`MAX_TRANSFER_CHUNK`] bytes as base64.
    Chunk {
        data: String,
        eof: bool,
    },
    /// `next_cursor`: the next page.
    Tree {
        nodes: Vec<TreeNode>,
        active_leaf: Option<String>,
        next_cursor: Option<String>,
        truncated: bool,
    },
    /// `dir`: `None` for the roots listing.
    Dir {
        dir: Option<String>,
        display: String,
        parent: Option<String>,
        entries: Vec<DirEntry>,
        truncated: bool,
    },
    Activity {
        days: Vec<DayCount>,
    },
    OmpSettings {
        groups: Vec<SettingsGroup>,
        truncated: bool,
    },
    BackupList {
        hosts: Vec<BackupHost>,
        truncated: bool,
    },
    SyncBackups {
        backups: Vec<SyncBackupView>,
        truncated: bool,
    },
    Upload {
        upload: String,
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
    // ---- slice 4 (sent to devices that announced [`FEATURE_S4`]) ----
    Text {
        text: String,
    },
    /// `running_kill`.
    Killed {
        results: Vec<KillResult>,
    },
    /// `backup_restore_prepare`/`sync_restore_prepare`: apply or discard `handle` (bound to
    /// this device).
    RestorePlan {
        handle: u64,
        plan: RestorePlanView,
        truncated: bool,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionResult {
    pub session: String,
    pub error: Option<String>,
}

/// [`DoneView::Killed`]: per running id, `None` = killed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KillResult {
    pub id: String,
    pub error: Option<String>,
}

// ---- slice 4: views (gateway → device) ----------------------------------------------------------

/// An image of a transcript message: `blob` (16 hex of SHA-256 over the stored data or
/// reference) is fetched with [`Op::Blob`]; `bytes` is `0` when unknown.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageRef {
    pub blob: String,
    pub mime: String,
    pub bytes: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UiKind {
    /// `options` (`multi`: several), `initial` preselected.
    Select,
    /// `title` and `message`, yes/no.
    Confirm,
    /// One line, `prefill`.
    Input,
    /// Multi-line, `prefill`.
    Editor,
    /// omp's `ask` tool: `questions`.
    Ask,
    /// A tool approval: `approve`.
    Approve,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiOption {
    pub label: String,
    pub description: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiQuestion {
    pub id: String,
    pub question: String,
    pub header: Option<String>,
    pub options: Vec<UiOption>,
    pub multi: bool,
    /// The recommended option index.
    pub recommended: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApproveGrant {
    /// omp's scope (`folder`, `target`).
    pub scope: String,
    pub label: String,
    pub always_label: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApproveView {
    pub full: String,
    pub compact: String,
    /// The "for this session" choice's label; `None`: not offered.
    pub session_label: Option<String>,
    pub grants: Vec<ApproveGrant>,
}

/// A pending omp UI request. Fields a kind does not use are empty/`None`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiRequestView {
    /// `"u-"` + the decimal ctl id.
    pub request: String,
    pub kind: UiKind,
    pub title: String,
    pub message: Option<String>,
    /// omp answers by itself after this long (the recommended or first option).
    pub timeout_ms: Option<u64>,
    pub options: Vec<UiOption>,
    pub multi: bool,
    /// The preselected option index.
    pub initial: Option<u32>,
    pub prefill: Option<String>,
    pub questions: Vec<UiQuestion>,
    pub approve: Option<ApproveView>,
}

/// One sub-folder of a [`Resp::Dir`] listing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirEntry {
    pub name: String,
    /// Its dir token.
    pub dir: String,
    /// Its project id when it holds omp sessions.
    pub project: Option<String>,
    /// It is a git repository.
    pub repo: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MuxBackend {
    Tmux,
    Host,
}

/// A running mux session ([`Feed::Running`]).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunningView {
    /// Running id (`"r-"` + 16 hex), what [`Op::RunningKill`] takes.
    pub id: String,
    /// omp session ids of the files its omp has open.
    pub sessions: Vec<String>,
    pub project: Option<String>,
    /// `~`-abbreviated working folder.
    pub display: Option<String>,
    pub title: Option<String>,
    pub backend: MuxBackend,
    pub started_unix_ms: Option<u64>,
    pub pid: Option<u32>,
    /// Memory of its whole process tree.
    pub memory_bytes: Option<u64>,
    pub processes: Option<u32>,
    /// The live session when the gateway controls it.
    pub live: Option<LiveRef>,
}

/// Statistics read from a session file.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct InventoryStats {
    pub user_messages: u32,
    pub assistant_turns: u32,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    /// Dollars.
    pub cost: Option<f64>,
    pub subagent_cost: Option<f64>,
    /// The most used model.
    pub model: Option<String>,
    pub compactions: u32,
    pub last_activity_unix_ms: Option<u64>,
}

/// One session file of [`Feed::Inventory`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct InventoryView {
    pub id: String,
    pub project: String,
    /// `~`-abbreviated cwd.
    pub display: String,
    pub title: Option<String>,
    pub first_prompt: Option<String>,
    pub stub: bool,
    pub created_unix_ms: Option<u64>,
    pub modified_unix_ms: u64,
    pub jsonl_bytes: u64,
    /// Sibling directory and `.bak` files.
    pub artifact_bytes: u64,
    pub artifact_files: u32,
    pub subagents: u32,
    /// The parent session's id.
    pub parent: Option<String>,
    pub cwd_exists: bool,
    pub running: bool,
    /// `None` until read.
    pub stats: Option<InventoryStats>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextCategory {
    pub id: String,
    pub label: String,
    pub tokens: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolTokens {
    pub name: String,
    pub tokens: u64,
}

/// omp's `/context` breakdown.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextView {
    pub model: Option<String>,
    pub context_window: u64,
    /// Provider-anchored total, not the sum of the categories.
    pub used_tokens: u64,
    pub anchored: bool,
    pub categories: Vec<ContextCategory>,
    pub auto_compact_buffer: u64,
    pub free_tokens: u64,
    pub threshold_tokens: Option<u64>,
    pub snapcompact_saved: Option<u64>,
    /// Per-tool schema tokens, descending.
    pub tools: Vec<ToolTokens>,
    pub skills: u32,
    pub computed_unix_ms: u64,
}

/// One quota of a provider report.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UsageLimitView {
    pub id: String,
    pub label: String,
    /// The window's label (`5h`, `weekly`, …).
    pub window: Option<String>,
    pub resets_unix_ms: Option<u64>,
    pub used: Option<f64>,
    pub limit: Option<f64>,
    pub remaining: Option<f64>,
    pub used_fraction: Option<f64>,
    pub remaining_fraction: Option<f64>,
    /// `percent`, `tokens`, `requests`, `credits`, `usd`, `minutes`, `bytes` or `unknown`.
    pub unit: String,
    /// omp's limit status.
    pub status: String,
}

/// A provider quota card.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UsageReportView {
    pub provider: String,
    pub account: Option<String>,
    pub plan: Option<String>,
    pub fetched_unix_ms: Option<u64>,
    pub limits: Vec<UsageLimitView>,
    pub notes: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UsageView {
    pub reports: Vec<UsageReportView>,
    /// The smallest `remaining_fraction` over every limit.
    pub tightest_remaining: Option<f64>,
    /// Accounts without usage data (labels).
    pub unavailable: Vec<String>,
}

/// One day of [`Resp::Activity`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DayCount {
    /// `YYYY-MM-DD`, local time.
    pub day: String,
    pub cost: Option<f64>,
    pub requests: u64,
    pub tokens: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    Running,
    Done,
    Failed,
    Aborted,
    Interrupted,
    /// Finished but revivable (kept alive by the live extension).
    Parked,
}

/// A subagent ([`Feed::Agents`]).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AgentView {
    /// e.g. `CoreStatus` or `M2cCore.M2cSaveService`.
    pub id: String,
    /// `None`: spawned by the main session.
    pub parent: Option<String>,
    pub depth: u32,
    /// The task gist.
    pub label: String,
    /// The agent kind (`task`, `scout`, …).
    pub agent: Option<String>,
    pub status: AgentStatus,
    pub model: Option<String>,
    pub thinking: Option<String>,
    pub error: Option<String>,
    /// While running: the pending tool.
    pub current: Option<String>,
    pub created_unix_ms: Option<u64>,
    pub last_activity_unix_ms: Option<u64>,
    pub duration_ms: Option<u64>,
    pub requests: u32,
    pub tool_calls: u32,
    pub tokens: u64,
    pub cost: Option<f64>,
    pub context_tokens: Option<u64>,
    /// It wrote an output file.
    pub has_output: bool,
    /// Reachable through live control (kill, revive, chat).
    pub live: bool,
}

/// The git repository of a project ([`Feed::Worktrees`]).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeView {
    /// The project is a linked worktree.
    pub linked: bool,
    pub branch: Option<String>,
    /// The recorded base of a linked worktree.
    pub base: Option<String>,
    /// The main checkout's project id, when it is a project.
    pub primary: Option<String>,
    pub primary_display: String,
    /// Local branches first, then remote-tracking ones.
    pub branches: Vec<String>,
    pub has_commits: bool,
}

/// A parked branch: its worktree is gone, the branch is kept.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParkedView {
    /// Opaque id for [`Op::WorktreeContinue`].
    pub id: String,
    pub branch: String,
    pub base: String,
    pub sessions: u32,
    pub parked_unix_ms: u64,
    pub exists: bool,
    pub merged: bool,
    /// Checked out in some working tree.
    pub checked_out: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncInterval {
    Off,
    TenMinutes,
    ThirtyMinutes,
    Hourly,
    SixHours,
    Daily,
}

/// One project sync row.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncRowView {
    pub project: String,
    pub display: String,
    pub parts: SyncParts,
    /// Session ids opted out.
    pub excluded: Vec<String>,
    pub last_backup_unix_ms: Option<u64>,
}

/// omp's version and update check.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OmpUpdateView {
    /// The installed omp version.
    pub version: Option<String>,
    /// The last check: the current and newer (`None`: up to date) versions, or its error.
    pub current: Option<String>,
    pub latest: Option<String>,
    pub error: Option<String>,
    pub checked_unix_ms: Option<u64>,
    pub checking: bool,
}

/// How this OmpDeck was installed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstallKind {
    UserDir,
    AppImage,
    Deb,
    Rpm,
    WindowsDir,
    Dev,
    Unsupported,
}

/// OmpDeck's own update (tag `state`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum UpdatePhase {
    Idle {},
    Checking {},
    Downloading {
        done: u64,
        total: Option<u64>,
    },
    /// Installed: the gateway restarts into it.
    Applied {},
    /// Must be installed at the desktop.
    NeedsManual {},
    Failed {
        message: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppUpdateView {
    pub install: InstallKind,
    /// [`AppUpdateStep::Apply`] is allowed from a device (in-place installs).
    pub apply_allowed: bool,
    pub current: String,
    /// A newer release.
    pub latest: Option<String>,
    pub page_url: Option<String>,
    pub asset: Option<String>,
    pub checked_unix_ms: Option<u64>,
    pub phase: UpdatePhase,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PushStateView {
    pub subscribed: bool,
    pub prefs: PushPrefs,
    pub since_unix_ms: Option<u64>,
}

/// The gateway's lifetime mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GatewayMode {
    Off,
    WhileOpen,
    Background,
}

/// omp's session-tree entry kinds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TreeNodeKind {
    User,
    Assistant,
    ToolResult,
    Developer,
    BashExecution,
    /// Another message role ([`TreeNode::subtype`]).
    OtherRole,
    /// [`TreeNode::subtype`]: its custom type.
    CustomMessage,
    Compaction,
    BranchSummary,
    ModelChange,
    ModelUsage,
    ThinkingChange,
    Label,
    /// [`TreeNode::subtype`]: its custom type.
    Custom,
    /// Any other entry type ([`TreeNode::subtype`]).
    Setting,
}

/// One session-tree entry ([`Resp::Tree`]).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TreeNode {
    pub id: String,
    pub parent: Option<String>,
    pub kind: TreeNodeKind,
    pub subtype: Option<String>,
    pub at_unix_ms: Option<u64>,
    /// omp's row text, at most [`TREE_TEXT_CHARS`] characters.
    pub text: String,
    pub has_text: bool,
    pub stop_reason: Option<String>,
    /// Its label, when labeled.
    pub label: Option<String>,
    /// A message entry, so [`Op::ForkAt`] can fork at it.
    pub forkable: bool,
    /// Recorded cost of every entry before it, dollars.
    pub cost_before: Option<f64>,
    /// `cost_before` is a lower bound.
    pub cost_incomplete: bool,
}

/// An omp setting's type (tag `type`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SettingKind {
    Bool {},
    Number {},
    Enum { options: Vec<String> },
    String {},
    Array {},
    Record {},
}

/// One omp setting. A secret never carries its value: `value` is `None` and `set` tells
/// whether one is stored.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SettingView {
    pub key: String,
    pub kind: SettingKind,
    pub description: String,
    pub value: Option<serde_json::Value>,
    pub default: Option<serde_json::Value>,
    /// The value differs from the default.
    pub modified: bool,
    pub secret: bool,
    pub set: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SettingsGroup {
    pub id: String,
    pub label: String,
    pub entries: Vec<SettingView>,
}

/// Where config backups go (the active destination); never a secret, password or path.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackupDestinationView {
    pub kind: BackupKind,
    /// `Google Drive`, or the server's `sftp://host[:port]`.
    pub label: String,
}

/// The kind of backup destination.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackupKind {
    Drive,
    Ftp,
    Ftps,
    Sftp,
}

/// One config backup at the active destination.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackupFileView {
    pub id: String,
    pub name: String,
    pub size: u64,
    pub created_unix_ms: Option<u64>,
}

/// The config backups of one machine, newest first.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackupHost {
    pub host: String,
    pub backups: Vec<BackupFileView>,
}

/// One project backup at the active destination.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncBackupView {
    pub id: String,
    /// The folder's name.
    pub name: String,
    pub host: String,
    /// The project folder on the uploading machine, `~`-abbreviated as recorded.
    pub display: String,
    pub size: u64,
    pub created_unix_ms: Option<u64>,
    pub omp_bytes: Option<u64>,
    pub memory_bytes: Option<u64>,
    pub sessions: Option<u64>,
    pub session_bytes: Option<u64>,
}

/// A local copy of a session id a project restore would touch.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalCopyView {
    pub project: Option<String>,
    pub display: Option<String>,
    pub modified_unix_ms: u64,
    pub bytes: u64,
}

/// A session a project restore adds; `local` non-empty = a conflict ([`ConflictChoice`]).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionPlanView {
    pub id: String,
    pub title: String,
    pub modified_unix_ms: u64,
    pub bytes: u64,
    pub local: Vec<LocalCopyView>,
    /// Why it cannot be restored.
    pub problem: Option<String>,
}

/// What a prepared restore would change (tag `type`); paths are `~` displays.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RestorePlanView {
    /// A config backup.
    Backup {
        name: String,
        created_unix_ms: Option<u64>,
        host: String,
        replaced: Vec<String>,
        added: Vec<String>,
        dirs: Vec<String>,
        removed: Vec<String>,
        /// Where the replaced files are saved first.
        safety_display: String,
    },
    /// A project backup.
    Project {
        target_display: String,
        create_target: bool,
        omp_exists: bool,
        omp_overwritten: Vec<String>,
        omp_added: Vec<String>,
        omp_skipped: Vec<String>,
        memory_exists: bool,
        memory_overwritten: Vec<String>,
        memory_added: Vec<String>,
        memory_skipped: Vec<String>,
        sessions: Vec<SessionPlanView>,
    },
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
    fn slice4_classes_journaling_and_bounds() {
        let live = LiveRef {
            key: "k".into(),
            generation: 1,
        };
        let chunk = |n: usize| Op::UploadChunk {
            upload: "x".into(),
            offset: 0,
            data: "A".repeat(n),
        };
        assert_eq!(chunk(0).class(), Class::Control);
        assert!(!chunk(0).journaled());
        assert!(!Op::UpdateCheck {}.journaled());
        assert!(!Op::Unpair {}.journaled());
        assert!(Op::BackupRun {}.journaled());
        assert_eq!(Op::BackupConnect {}.class(), Class::Manage);
        assert_eq!(
            Op::AnswerUi {
                live,
                request: "u-1".into(),
                value: UiAnswer::Cancel {},
            }
            .allowed(Permissions::default()),
            Err(Class::Control)
        );
        assert!(chunk(MAX_TRANSFER_CHUNK_B64).check().is_ok());
        assert!(chunk(MAX_TRANSFER_CHUNK_B64 + 1).check().is_err());
        let tree = |limit| Op::Tree {
            session: "s".into(),
            filter: TreeFilter::All,
            cursor: None,
            limit,
        };
        assert!(tree(0).check().is_err());
        assert!(tree(MAX_TREE_LIMIT).check().is_ok());
        assert!(tree(MAX_TREE_LIMIT + 1).check().is_err());
        let days = |days| Op::UsageActivity { days };
        assert!(days(0).check().is_err());
        assert!(days(MAX_ACTIVITY_DAYS).check().is_ok());
        assert!(days(MAX_ACTIVITY_DAYS + 1).check().is_err());
        let inventory = |limit| Op::Subscribe {
            feed: Feed::Inventory {
                project: None,
                limit,
            },
        };
        assert!(inventory(0).check().is_err());
        assert!(inventory(MAX_INVENTORY_LIMIT).check().is_ok());
        assert!(inventory(MAX_INVENTORY_LIMIT + 1).check().is_err());
        assert!(!PushPrefs::default().show_titles && PushPrefs::default().ended);
        assert_eq!(MAX_TRANSFER_CHUNK_B64 % 4, 0);
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
