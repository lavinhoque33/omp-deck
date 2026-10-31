//! Native Agent Hub data layer: subagent summaries of the active session and their live
//! transcripts, read-only from omp's files (docs/notes/agent-hub.md).
//!
//! * [`AgentsWatcher`] rescans `<session stem>/` and tails every transcript incrementally.
//! * [`TranscriptWatcher`] follows one transcript and maps it to [`TranscriptItem`]s.
//! * [`AgentsScanner`] / [`TranscriptReader`] are the synchronous cores of the two watchers.

use std::path::PathBuf;
use std::time::{Duration, SystemTime};

pub mod markup;
mod scan;
pub(crate) mod tail;
mod transcript;
mod watcher;

pub use scan::AgentsScanner;
pub use transcript::{TranscriptReader, TranscriptUpdate, TranscriptWatcher, read_blob};
pub use watcher::AgentsWatcher;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum AgentStatus {
    Running,
    Done,
    Failed,
    Aborted,
    Interrupted,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct AgentSummary {
    /// File stem, e.g. "CoreStatus" or "M2cCore.M2cSaveService".
    pub id: String,
    /// `None` = spawned by the main session.
    pub parent_id: Option<String>,
    pub depth: usize,
    /// Transcript `.jsonl`.
    pub path: PathBuf,
    pub advisor: bool,
    /// `session_init.agent` ("task", "scout", …).
    pub agent: Option<String>,
    /// `provider/id` (latest model_change, else resolvedModel).
    pub model: Option<String>,
    pub thinking: Option<String>,
    /// Preamble stripped, `# Target` section preferred, ≤ 1000 chars.
    pub task_gist: String,
    /// Full assignment text (≤ 64 KiB).
    pub task: String,
    pub status: AgentStatus,
    /// Last errorMessage for Failed/Aborted.
    pub error: Option<String>,
    /// Running: pending tool ("bash · cargo test", intent preferred).
    pub current: Option<String>,
    pub created: Option<SystemTime>,
    pub last_activity: Option<SystemTime>,
    /// Last entry − header timestamp.
    pub duration: Option<Duration>,
    pub requests: u32,
    pub tool_calls: u32,
    pub tokens: u64,
    pub cost: f64,
    pub context_tokens: Option<u64>,
    /// `<id>.md` when its size > 0.
    pub output_path: Option<PathBuf>,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct AgentsTarget {
    pub session: PathBuf,
    /// The tab's omp process is running.
    pub owner_alive: bool,
}

#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct AgentsSnapshot {
    pub session: PathBuf,
    /// Parent before child, newest first among siblings.
    pub agents: Vec<AgentSummary>,
}

/// An image carried by a transcript message. `blob` is the first 16 hex digits of SHA-256 over
/// the image's base64 `data` string exactly as stored in the transcript (for images omp
/// externalized, the `blob:sha256:…` reference string); [`read_blob`] resolves it.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ImageRef {
    pub blob: String,
    pub mime: String,
    /// Decoded size; `0` for an externalized image whose size is not known without reading it.
    pub bytes: u64,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum TranscriptItem {
    User {
        text: String,
        from_agent: bool,
        at: Option<SystemTime>,
        #[serde(default)]
        images: Vec<ImageRef>,
    },
    Assistant {
        text: String,
        model: Option<String>,
        at: Option<SystemTime>,
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
        /// Structured arguments: string leaves cut to 16 KiB (`…` appended); `None` when the
        /// value is still over 64 KiB serialized.
        #[serde(default)]
        args_value: Option<serde_json::Value>,
    },
    ToolResult {
        call_id: String,
        name: String,
        text: String,
        is_error: bool,
        truncated: bool,
        /// omp's tool-result `details`; `None` when absent or over 32 KiB serialized.
        #[serde(default)]
        details: Option<serde_json::Value>,
        #[serde(default)]
        images: Vec<ImageRef>,
    },
    /// custom_message display:true (async-result, …).
    Notice {
        kind: String,
        text: String,
    },
    /// An `<irc>` block from a steering message or an `irc:*` notice; `text` is its markdown body.
    Irc {
        /// `from` attribute (`parent`, …).
        from: Option<String>,
        /// Sending agent (`agent` attribute, or the notice's "from agent `X`" line).
        agent: Option<String>,
        text: String,
        /// A bracketed line before the block, e.g. "Wait interrupted by message".
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
    /// Assistant stopReason error/aborted.
    Error {
        message: String,
    },
    Exit {
        reason: String,
    },
}
