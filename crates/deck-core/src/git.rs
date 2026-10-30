//! Shared git runner: one place that builds `git` commands (`LC_ALL=C`, no prompts, no console
//! window) and runs them through the bounded, tree-killing runner of [`crate::usage`].
//!
//! Everything here blocks; callers run it on workers.

use std::ffi::OsStr;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use crate::usage::{UsageError, run_bounded_bytes};

/// Output cap per stream for ordinary commands.
const DEFAULT_CAP: u64 = 16 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub(crate) enum GitError {
    #[error("git timed out")]
    Timeout,
    #[error("could not run git: {0}")]
    Spawn(#[source] std::io::Error),
}

impl From<UsageError> for GitError {
    fn from(e: UsageError) -> Self {
        match e {
            UsageError::Timeout(_) => GitError::Timeout,
            UsageError::Spawn(e) => GitError::Spawn(e),
            // `run_bounded_bytes` only produces the two variants above.
            other => GitError::Spawn(std::io::Error::other(other.to_string())),
        }
    }
}

/// Result of a git run with raw stdout (paths and diffs need not be UTF-8).
pub(crate) struct GitBytes {
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: String,
}

impl GitBytes {
    pub fn ok(&self) -> bool {
        self.code == Some(0)
    }

    /// Stderr, falling back to stdout, trimmed: the text to show for a failed command.
    pub fn message(&self) -> String {
        message(&self.stderr, &String::from_utf8_lossy(&self.stdout))
    }
}

/// Result of a git run with lossy UTF-8 stdout.
pub(crate) struct GitOut {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl GitOut {
    pub fn ok(&self) -> bool {
        self.code == Some(0)
    }

    pub fn message(&self) -> String {
        message(&self.stderr, &self.stdout)
    }
}

fn message(stderr: &str, stdout: &str) -> String {
    let stderr = stderr.trim();
    if stderr.is_empty() {
        stdout.trim().to_owned()
    } else {
        stderr.to_owned()
    }
}

/// Global options that keep git from running the repository's `core.fsmonitor` hook program.
pub(crate) const NO_FSMONITOR: [&str; 2] = ["-c", "core.fsmonitor=false"];

/// `git [--no-optional-locks] -c core.fsmonitor=false -C <cwd> <args>` with the OmpDeck
/// environment. `read_only` commands skip the index lock so they never race the user's own git.
/// `core.fsmonitor` is forced off for every command: a repository's own config could otherwise
/// name a program git runs on status, diff and add, and browsing a folder must never execute it.
pub(crate) fn command<S: AsRef<OsStr>>(cwd: &Path, args: &[S], read_only: bool) -> Command {
    let mut cmd = Command::new("git");
    if read_only {
        cmd.arg("--no-optional-locks");
    }
    cmd.args(NO_FSMONITOR)
        .arg("-C")
        .arg(cwd)
        .args(args)
        .env("LC_ALL", "C")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    crate::platform::no_window(&mut cmd);
    cmd
}

/// Runs `cmd` keeping at most `cap` bytes of each stream; returns whatever the exit code.
pub(crate) fn run_bytes(cmd: Command, timeout: Duration, cap: u64) -> Result<GitBytes, GitError> {
    let (code, stdout, stderr) = run_bounded_bytes(cmd, timeout, cap)?;
    Ok(GitBytes {
        code,
        stdout,
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
    })
}

/// Runs git and returns its output whatever the exit code (callers interpret it).
pub(crate) fn raw<S: AsRef<OsStr>>(
    cwd: &Path,
    args: &[S],
    read_only: bool,
    timeout: Duration,
) -> Result<GitOut, GitError> {
    let out = run_bytes(command(cwd, args, read_only), timeout, DEFAULT_CAP)?;
    Ok(GitOut {
        code: out.code,
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: out.stderr,
    })
}

/// A path printed by git (forward slashes on Windows) as a native path.
pub(crate) fn native(printed: &str) -> std::path::PathBuf {
    #[cfg(windows)]
    let printed = printed.replace('/', "\\");
    std::path::PathBuf::from(printed)
}
