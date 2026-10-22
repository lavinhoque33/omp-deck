//! Native Agent Hub data layer: subagent summaries of the active session and their live
//! transcripts, read-only from omp's files (docs/notes/agent-hub.md).
//!
//! * [`AgentsWatcher`] rescans `<session stem>/` and tails every transcript incrementally.
//! * [`TranscriptWatcher`] follows one transcript and maps it to [`TranscriptItem`]s.
//! * [`AgentsScanner`] / [`TranscriptReader`] are the synchronous cores of the two watchers.

use std::path::PathBuf;
use std::time::{Duration, SystemTime};

mod scan;
mod tail;
mod transcript;
mod watcher;

pub use scan::AgentsScanner;
pub use transcript::{TranscriptReader, TranscriptUpdate, TranscriptWatcher};
pub use watcher::AgentsWatcher;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AgentStatus {
    Running,
    Done,
    Failed,
    Aborted,
    Interrupted,
}

#[derive(Clone, Debug, PartialEq)]
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

#[derive(Clone, Debug, PartialEq)]
pub struct AgentsTarget {
    pub session: PathBuf,
    /// The tab's omp process is running.
    pub owner_alive: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct AgentsSnapshot {
    pub session: PathBuf,
    /// Parent before child, newest first among siblings.
    pub agents: Vec<AgentSummary>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum TranscriptItem {
    User {
        text: String,
        from_agent: bool,
        at: Option<SystemTime>,
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
    },
    ToolResult {
        call_id: String,
        name: String,
        text: String,
        is_error: bool,
        truncated: bool,
    },
    /// custom_message display:true, async-result, irc.
    Notice {
        kind: String,
        text: String,
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
