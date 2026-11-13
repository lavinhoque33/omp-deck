//! Repository checkpoints stored under `refs/ompdeck/checkpoints`, without changing the user's
//! index. All operations block; callers must run them on a worker and quiesce repository writers
//! before restore. Ignored content and unchanged submodules are never removed.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::git;

const TIMEOUT: Duration = Duration::from_secs(120);
const OUTPUT_CAP: u64 = 64 * 1024 * 1024;
const KEEP: usize = 50;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckpointKind {
    Turn,
    Rollback,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Checkpoint {
    pub seq: u32,
    pub commit: String,
    pub at_unix_ms: u64,
    pub prompt: String,
    pub entry: Option<String>,
    pub kind: CheckpointKind,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileDelta {
    pub path: String,
    pub old_path: Option<String>,
    pub added: Option<u32>,
    pub removed: Option<u32>,
    pub status: char,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestoreReport {
    pub changed: u32,
}

#[derive(Debug, thiserror::Error)]
pub enum CheckpointError {
    #[error("not a git working tree: {0}")]
    NotRepository(PathBuf),
    #[error("invalid checkpoint session id")]
    InvalidSessionId,
    #[error("checkpoint sequence exhausted")]
    SequenceExhausted,
    #[error("{command} failed: {stderr}")]
    Git { command: String, stderr: String },
    #[error("git timed out")]
    Timeout,
    #[error("checkpoint I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid git checkpoint data: {0}")]
    InvalidData(String),
    #[error("rollback would overwrite ignored path: {path}")]
    IgnoredCollision { path: PathBuf },
    #[error("rollback would change submodule: {path}")]
    GitlinkChanged { path: PathBuf },
    #[error("unsafe checkpoint path: {0}")]
    UnsafePath(PathBuf),
    #[error("rollback refuses a symlinked or reparse-point parent: {path}")]
    UnsafeParent { path: PathBuf },
}

pub type Result<T, E = CheckpointError> = std::result::Result<T, E>;

impl From<git::GitError> for CheckpointError {
    fn from(error: git::GitError) -> Self {
        match error {
            git::GitError::Timeout => Self::Timeout,
            git::GitError::Spawn(error) => Self::Io(error),
        }
    }
}

fn run<S: AsRef<OsStr>>(repo: &Path, args: &[S], index: Option<&Path>) -> Result<git::GitBytes> {
    let mut command = git::command(repo, args, true);
    command.env_remove("GIT_INDEX_FILE");
    if let Some(index) = index {
        command.env("GIT_INDEX_FILE", index);
    }
    command
        .env("GIT_AUTHOR_NAME", "OmpDeck")
        .env("GIT_AUTHOR_EMAIL", "checkpoints@omp-deck.local")
        .env("GIT_COMMITTER_NAME", "OmpDeck")
        .env("GIT_COMMITTER_EMAIL", "checkpoints@omp-deck.local");
    Ok(git::run_bytes(command, TIMEOUT, OUTPUT_CAP)?)
}

fn checked<S: AsRef<OsStr>>(repo: &Path, args: &[S], index: Option<&Path>) -> Result<Vec<u8>> {
    let output = run(repo, args, index)?;
    if !output.ok() {
        return Err(CheckpointError::Git {
            command: format!(
                "git {}",
                args.first()
                    .map_or_else(|| "".into(), |a| a.as_ref().to_string_lossy())
            ),
            stderr: output.message(),
        });
    }
    if output.stdout.len() as u64 >= OUTPUT_CAP {
        return Err(CheckpointError::InvalidData(
            "git output exceeded the checkpoint limit".into(),
        ));
    }
    Ok(output.stdout)
}

fn text(bytes: Vec<u8>) -> Result<String> {
    String::from_utf8(bytes)
        .map(|s| s.trim_end_matches(['\r', '\n']).to_owned())
        .map_err(|_| CheckpointError::InvalidData("expected UTF-8 git metadata".into()))
}

/// Working-tree root (including a linked worktree), or `None` outside a repository.
pub fn repo_root(cwd: &Path) -> Option<PathBuf> {
    let output = run(cwd, &["rev-parse", "--show-toplevel"], None).ok()?;
    if !output.ok() {
        return None;
    }
    let root = path_from_bytes(output.stdout.strip_suffix(b"\n").unwrap_or(&output.stdout));
    fs::canonicalize(root).ok()
}

fn root(repo: &Path) -> Result<PathBuf> {
    repo_root(repo).ok_or_else(|| CheckpointError::NotRepository(repo.to_owned()))
}

fn prefix(session_id: &str) -> Result<String> {
    if session_id.is_empty()
        || session_id.len() > 200
        || !session_id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_'))
    {
        return Err(CheckpointError::InvalidSessionId);
    }
    Ok(format!("refs/ompdeck/checkpoints/{session_id}/"))
}

struct Reference {
    seq: u32,
    commit: String,
    message: String,
}

fn references(repo: &Path, prefix: &str) -> Result<Vec<Reference>> {
    let bytes = checked(
        repo,
        &[
            "for-each-ref",
            "--format=%(refname)%00%(objectname)%00%(contents)%00",
            prefix,
        ],
        None,
    )?;
    let mut fields = bytes.split(|b| *b == 0);
    let mut refs = Vec::new();
    while let Some(name) = fields.next() {
        let Some(commit) = fields.next() else { break };
        let Some(message) = fields.next() else { break };
        let name = String::from_utf8_lossy(name);
        let Some(seq) = name
            .trim_start_matches(['\r', '\n'])
            .strip_prefix(prefix)
            .and_then(|s| s.parse().ok())
        else {
            log::warn!("checkpoints: skipped malformed ref {name:?}");
            continue;
        };
        refs.push(Reference {
            seq,
            commit: text(commit.to_vec())?,
            message: String::from_utf8_lossy(message).into_owned(),
        });
    }
    refs.sort_unstable_by_key(|reference| std::cmp::Reverse(reference.seq));
    Ok(refs)
}

fn checkpoint(reference: Reference) -> Option<Checkpoint> {
    let mut lines = reference.message.lines();
    if lines.next()? != "ompdeck checkpoint" {
        return None;
    }
    let mut at = None;
    let mut prompt = String::new();
    let mut entry = None;
    let mut kind = None;
    for line in lines {
        let Some((key, value)) = line.split_once(": ") else {
            continue;
        };
        match key {
            "at" => at = value.parse().ok(),
            "prompt" => prompt = value.to_owned(),
            "entry" if !value.is_empty() => entry = Some(value.to_owned()),
            "kind" => {
                kind = match value {
                    "turn" => Some(CheckpointKind::Turn),
                    "rollback" => Some(CheckpointKind::Rollback),
                    _ => None,
                }
            }
            _ => {}
        }
    }
    Some(Checkpoint {
        seq: reference.seq,
        commit: reference.commit,
        at_unix_ms: at?,
        prompt,
        entry,
        kind: kind?,
    })
}

/// Checkpoints of this session, newest sequence first. Malformed foreign refs are logged and skipped.
pub fn list(repo: &Path, session_id: &str) -> Result<Vec<Checkpoint>> {
    let prefix = prefix(session_id)?;
    let repo = root(repo)?;
    Ok(references(&repo, &prefix)?
        .into_iter()
        .filter_map(|reference| {
            let seq = reference.seq;
            let parsed = checkpoint(reference);
            if parsed.is_none() {
                log::warn!("checkpoints: skipped malformed checkpoint {prefix}{seq:06}");
            }
            parsed
        })
        .collect())
}

struct Snapshot {
    directory: tempfile::TempDir,
    index: PathBuf,
    tree: String,
}

fn clear_assume_unchanged(repo: &Path, index: &Path, directory: &Path) -> Result<()> {
    let listed = checked(repo, &["ls-files", "-v", "-z"], Some(index))?;
    let mut paths = Vec::new();
    for record in listed
        .split(|b| *b == 0)
        .filter(|record| !record.is_empty())
    {
        if record[0].is_ascii_lowercase() {
            if record.get(1) != Some(&b' ') {
                return Err(CheckpointError::InvalidData("malformed index flags".into()));
            }
            paths.extend_from_slice(&record[2..]);
            paths.push(0);
        }
    }
    if paths.is_empty() {
        return Ok(());
    }
    // NUL stdin preserves arbitrary git paths and avoids Windows' argument-length limit.
    let input = directory.join("assumed-paths");
    fs::write(&input, paths)?;
    let mut command = git::command(
        repo,
        &["update-index", "--no-assume-unchanged", "-z", "--stdin"],
        true,
    );
    command
        .env("GIT_INDEX_FILE", index)
        .stdin(fs::File::open(input)?);
    let output = git::run_bytes(command, TIMEOUT, OUTPUT_CAP)?;
    if !output.ok() {
        return Err(CheckpointError::Git {
            command: "git update-index --no-assume-unchanged".into(),
            stderr: output.message(),
        });
    }
    Ok(())
}

fn snapshot(repo: &Path) -> Result<Snapshot> {
    let directory = tempfile::tempdir()?;
    let index = directory.path().join("index");
    let printed_index = checked(
        repo,
        &["rev-parse", "--path-format=absolute", "--git-path", "index"],
        None,
    )?;
    let real_index = path_from_bytes(printed_index.strip_suffix(b"\n").unwrap_or(&printed_index));
    match fs::copy(real_index, &index) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            checked(repo, &["read-tree", "--empty"], Some(&index))?;
        }
        Err(error) => return Err(error.into()),
    }
    clear_assume_unchanged(repo, &index, directory.path())?;
    checked(repo, &["add", "-A", "--", "."], Some(&index))?;
    let tree = text(checked(repo, &["write-tree"], Some(&index))?)?;
    Ok(Snapshot {
        directory,
        index,
        tree,
    })
}

fn tree(repo: &Path, revision: &str) -> Result<String> {
    text(checked(
        repo,
        &[
            "rev-parse",
            "--verify",
            "--end-of-options",
            &format!("{revision}^{{tree}}"),
        ],
        None,
    )?)
}

fn prompt_line(prompt: &str) -> String {
    let mut result = String::new();
    let mut count = 0;
    for ch in prompt
        .chars()
        .filter(|ch| !ch.is_control() || ch.is_whitespace())
    {
        if count == 120 {
            break;
        }
        if ch.is_whitespace() {
            if !result.is_empty() && !result.ends_with(' ') {
                result.push(' ');
                count += 1;
            }
        } else {
            result.push(ch);
            count += 1;
        }
    }
    if result.ends_with(' ') {
        result.pop();
    }
    result
}

/// Take a rollback safety copy (or explicit turn checkpoint). `None` means the newest tree is unchanged.
/// The extension additionally records the originating user entry; explicit captures have no entry id.
pub fn create(
    repo: &Path,
    session_id: &str,
    kind: CheckpointKind,
    prompt: &str,
) -> Result<Option<Checkpoint>> {
    let prefix = prefix(session_id)?;
    let repo = root(repo)?;
    let refs = references(&repo, &prefix)?;
    let current = snapshot(&repo)?;
    if let Some(latest) = refs.first()
        && tree(&repo, &latest.commit)? == current.tree
    {
        return Ok(None);
    }
    let seq = refs
        .first()
        .map_or(Some(1), |r| r.seq.checked_add(1))
        .ok_or(CheckpointError::SequenceExhausted)?;
    let at_unix_ms = u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
    )
    .map_err(|_| CheckpointError::InvalidData("timestamp overflow".into()))?;
    let prompt = prompt_line(prompt);
    let label = match kind {
        CheckpointKind::Turn => "turn",
        CheckpointKind::Rollback => "rollback",
    };
    let message = format!(
        "ompdeck checkpoint\n\nsession: {session_id}\nentry: \nprompt: {prompt}\nkind: {label}\nat: {at_unix_ms}\n"
    );
    let mut args = vec!["commit-tree".to_owned(), current.tree];
    let head = run(&repo, &["rev-parse", "--verify", "HEAD"], None)?;
    if head.ok() {
        args.extend(["-p".into(), text(head.stdout)?]);
    }
    args.extend(["-m".into(), message]);
    let commit = text(checked(&repo, &args, None)?)?;
    let reference = format!("{prefix}{seq:06}");
    let zero = "0".repeat(commit.len());
    checked(&repo, &["update-ref", &reference, &commit, &zero], None)?;
    for old in refs.iter().skip(KEEP - 1) {
        checked(
            &repo,
            &[
                "update-ref",
                "-d",
                &format!("{prefix}{:06}", old.seq),
                &old.commit,
            ],
            None,
        )?;
    }
    Ok(Some(Checkpoint {
        seq,
        commit,
        at_unix_ms,
        prompt,
        entry: None,
        kind,
    }))
}

fn diff_trees(repo: &Path, checkpoint: &str, current: &str) -> Result<Vec<FileDelta>> {
    let flags = [
        "diff",
        "--no-ext-diff",
        "--no-textconv",
        "--find-renames",
        "-z",
    ];
    let mut args = flags.to_vec();
    args.extend(["--numstat", checkpoint, current, "--"]);
    let bytes = checked(repo, &args, None)?;
    let mut counts = BTreeMap::new();
    let mut fields = bytes.split(|b| *b == 0);
    while let Some(record) = fields.next().filter(|v| !v.is_empty()) {
        let mut parts = record.splitn(3, |b| *b == b'\t');
        let added = parts
            .next()
            .and_then(|v| std::str::from_utf8(v).ok())
            .and_then(|v| v.parse().ok());
        let removed = parts
            .next()
            .and_then(|v| std::str::from_utf8(v).ok())
            .and_then(|v| v.parse().ok());
        let path = parts
            .next()
            .ok_or_else(|| CheckpointError::InvalidData("malformed numstat".into()))?;
        let path = if path.is_empty() {
            fields
                .next()
                .ok_or_else(|| CheckpointError::InvalidData("missing rename source".into()))?;
            fields
                .next()
                .ok_or_else(|| CheckpointError::InvalidData("missing rename target".into()))?
        } else {
            path
        };
        counts.insert(path.to_vec(), (added, removed));
    }
    let mut args = flags.to_vec();
    args.extend(["--name-status", checkpoint, current, "--"]);
    let bytes = checked(repo, &args, None)?;
    let mut fields = bytes.split(|b| *b == 0);
    let mut result = Vec::new();
    while let Some(status) = fields.next().filter(|v| !v.is_empty()) {
        let status = char::from(status[0]);
        let first = fields
            .next()
            .ok_or_else(|| CheckpointError::InvalidData("missing diff path".into()))?;
        let (old_path, path) = if matches!(status, 'R' | 'C') {
            (
                Some(String::from_utf8_lossy(first).into_owned()),
                fields
                    .next()
                    .ok_or_else(|| CheckpointError::InvalidData("missing renamed path".into()))?,
            )
        } else {
            (None, first)
        };
        let (added, removed) = counts.remove(path).unwrap_or((None, None));
        result.push(FileDelta {
            path: String::from_utf8_lossy(path).into_owned(),
            old_path,
            added,
            removed,
            status,
        });
    }
    Ok(result)
}

/// Changes since a checkpoint, including current untracked files and binary/mode-only changes.
pub fn diff_stat(repo: &Path, commit: &str) -> Result<Vec<FileDelta>> {
    let repo = root(repo)?;
    let target = tree(&repo, commit)?;
    let current = snapshot(&repo)?;
    diff_trees(&repo, &target, &current.tree)
}

struct TreeEntry {
    path: PathBuf,
    mode: String,
    object: String,
}

fn path_from_bytes(bytes: &[u8]) -> PathBuf {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        PathBuf::from(OsStr::from_bytes(bytes))
    }
    #[cfg(not(unix))]
    {
        PathBuf::from(String::from_utf8_lossy(bytes).trim_end_matches('\r'))
    }
}

fn safe_path(bytes: &[u8]) -> Result<PathBuf> {
    let path = path_from_bytes(bytes);
    if path.as_os_str().is_empty()
        || path.components().any(|part| match part {
            Component::Normal(name) => name.as_encoded_bytes().eq_ignore_ascii_case(b".git"),
            _ => true,
        })
    {
        return Err(CheckpointError::UnsafePath(path));
    }
    Ok(path)
}

fn entries(repo: &Path, tree: &str) -> Result<Vec<TreeEntry>> {
    let bytes = checked(repo, &["ls-tree", "-r", "-z", tree], None)?;
    bytes
        .split(|b| *b == 0)
        .filter(|v| !v.is_empty())
        .map(|record| {
            let tab = record
                .iter()
                .position(|b| *b == b'\t')
                .ok_or_else(|| CheckpointError::InvalidData("malformed tree entry".into()))?;
            let mut header = record[..tab].split(|b| *b == b' ');
            let mode = header
                .next()
                .ok_or_else(|| CheckpointError::InvalidData("missing tree mode".into()))?;
            header.next();
            let object = header
                .next()
                .ok_or_else(|| CheckpointError::InvalidData("missing tree object".into()))?;
            Ok(TreeEntry {
                path: safe_path(&record[tab + 1..])?,
                mode: text(mode.to_vec())?,
                object: text(object.to_vec())?,
            })
        })
        .collect()
}

/// Compare complete components, not string prefixes. Windows folds case conservatively so
/// ignored files cannot be overwritten through a differently cased target spelling.
fn paths_overlap(left: &Path, right: &Path, ignore_case: bool) -> bool {
    let mut left = left.components();
    let mut right = right.components();
    loop {
        match (left.next(), right.next()) {
            (Some(left), Some(right)) => {
                let left = left.as_os_str();
                let right = right.as_os_str();
                if left != right
                    && (!ignore_case
                        || !left
                            .to_string_lossy()
                            .chars()
                            .flat_map(char::to_uppercase)
                            .eq(right.to_string_lossy().chars().flat_map(char::to_uppercase)))
                {
                    return false;
                }
            }
            _ => return true,
        }
    }
}

fn preflight_parents(repo: &Path, relative: &Path) -> Result<()> {
    let mut path = repo.to_owned();
    let mut components = relative.components().peekable();
    while let Some(component) = components.next() {
        if components.peek().is_none() {
            break;
        }
        path.push(component);
        match fs::symlink_metadata(&path) {
            Ok(metadata) => {
                #[cfg(windows)]
                let reparse = {
                    use std::os::windows::fs::MetadataExt;
                    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
                    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
                };
                #[cfg(not(windows))]
                let reparse = false;
                if metadata.file_type().is_symlink() || reparse {
                    return Err(CheckpointError::UnsafeParent { path });
                }
                // A regular file can legitimately become a directory during rollback. There
                // cannot be an existing deeper ancestor below it; deletion happens only later.
                if !metadata.is_dir() {
                    break;
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) =>
            {
                break;
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

/// Restore nonignored files exactly, preserving the real index. Symlinked/reparse parents,
/// ignored collisions and changed submodules are refused before working-tree mutation.
pub fn restore(repo: &Path, commit: &str) -> Result<RestoreReport> {
    let repo = root(repo)?;
    let target = tree(&repo, commit)?;
    let target_entries = entries(&repo, &target)?;
    for entry in &target_entries {
        preflight_parents(&repo, &entry.path)?;
    }
    // Check even stale/assume-unchanged index paths before the current-state snapshot refreshes
    // them, so an indexed descendant of a replaced directory never traverses outside the repo.
    let indexed = checked(&repo, &["ls-files", "-z"], None)?;
    for path in indexed.split(|b| *b == 0).filter(|path| !path.is_empty()) {
        preflight_parents(&repo, &safe_path(path)?)?;
    }
    let current = snapshot(&repo)?;
    let mut current_entries = entries(&repo, &current.tree)?;
    for entry in &current_entries {
        preflight_parents(&repo, &entry.path)?;
    }
    let links = |items: &[TreeEntry]| {
        items
            .iter()
            .filter(|e| e.mode == "160000")
            .map(|e| (e.path.clone(), e.object.clone()))
            .collect::<BTreeMap<_, _>>()
    };
    let current_links = links(&current_entries);
    let target_links = links(&target_entries);
    for path in current_links.keys().chain(target_links.keys()) {
        if current_links.get(path) != target_links.get(path) {
            return Err(CheckpointError::GitlinkChanged { path: path.clone() });
        }
    }
    let ignored = checked(
        &repo,
        &[
            "ls-files",
            "--others",
            "--ignored",
            "--exclude-standard",
            "-z",
        ],
        Some(&current.index),
    )?;
    for path in ignored.split(|b| *b == 0).filter(|v| !v.is_empty()) {
        let path = safe_path(path)?;
        if target_entries
            .iter()
            .any(|entry| paths_overlap(&entry.path, &path, cfg!(windows)))
        {
            return Err(CheckpointError::IgnoredCollision { path });
        }
    }
    let changed = u32::try_from(diff_trees(&repo, &target, &current.tree)?.len())
        .map_err(|_| CheckpointError::InvalidData("too many changed files".into()))?;
    current_entries.sort_unstable_by(|a, b| {
        b.path
            .components()
            .count()
            .cmp(&a.path.components().count())
            .then_with(|| b.path.cmp(&a.path))
    });
    let mut parents = BTreeSet::new();
    for entry in &current_entries {
        if entry.mode == "160000" {
            continue;
        }
        let path = repo.join(&entry.path);
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_file() || metadata.file_type().is_symlink() => {
                fs::remove_file(&path)?
            }
            Ok(_) => return Err(CheckpointError::UnsafePath(entry.path.clone())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let mut parent = entry.path.parent();
        while let Some(path) = parent.filter(|p| !p.as_os_str().is_empty()) {
            parents.insert(path.to_owned());
            parent = path.parent();
        }
    }
    let mut parents: Vec<_> = parents.into_iter().collect();
    parents.sort_unstable_by_key(|p| std::cmp::Reverse(p.components().count()));
    for parent in parents {
        let _ = fs::remove_dir(repo.join(parent));
    }
    let index = current.directory.path().join("restore-index");
    checked(&repo, &["read-tree", &target], Some(&index))?;
    checked(&repo, &["checkout-index", "-a", "-f"], Some(&index))?;
    Ok(RestoreReport { changed })
}

#[cfg(test)]
mod tests;
