//! Working-tree changes of a git repository for the review panel: `status`, per-file diffs and
//! the write actions (stage / unstage / discard / delete untracked).
//!
//! Everything blocks (git child processes); callers run it on workers. Every path operand is
//! passed after `--` with `GIT_LITERAL_PATHSPECS=1`, so a file called `*` or `:(exclude)x` only
//! ever means itself. Write actions re-verify the repository root first, so a stale panel can
//! never act on a different repository than the one it displays.

use std::ffi::{OsStr, OsString};
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use crate::git::{self, GitBytes, native};

const READ_TIMEOUT: Duration = Duration::from_secs(15);
const SCAN_TIMEOUT: Duration = Duration::from_secs(30);
const WRITE_TIMEOUT: Duration = Duration::from_secs(120);
/// Largest diff text kept, in bytes.
const MAX_DIFF_BYTES: usize = 2 * 1024 * 1024;
/// Largest diff text kept, in lines.
const MAX_DIFF_LINES: usize = 20_000;
/// Largest copied patch, in bytes.
const MAX_PATCH_BYTES: usize = 8 * 1024 * 1024;
/// Binary sniff window (git's own: a NUL in the first 8000 bytes).
const BINARY_SNIFF: usize = 8000;
/// Combined length of path operands per git invocation (Windows' command line is ~32 KiB).
const MAX_OPERAND_BYTES: usize = 12_000;
/// Flags that make `git diff` output independent of the user's configuration.
const DIFF_FLAGS: [&str; 15] = [
    "-c",
    "core.quotepath=off",
    "-c",
    "color.ui=never",
    "-c",
    "diff.noprefix=false",
    "-c",
    "diff.mnemonicPrefix=false",
    "--no-pager",
    "diff",
    "--no-ext-diff",
    "--no-textconv",
    "--no-color",
    "--submodule=short",
    "-U3",
];

/// Kind of change on one side (index vs HEAD, or worktree vs index).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Change {
    Modified,
    Added,
    Deleted,
    Renamed,
    Copied,
    TypeChanged,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangedFile {
    /// Display path, repo-relative with `/` separators, lossy UTF-8 (untracked directories end
    /// in `/`).
    pub path: String,
    /// Exact path for commands (same relative form as `path`, byte-exact).
    pub os_path: OsString,
    /// Rename / copy source.
    pub orig_path: Option<String>,
    pub staged: Option<Change>,
    pub unstaged: Option<Change>,
    pub untracked: bool,
    pub conflicted: bool,
    /// An untracked directory (git reports it as one entry).
    pub is_dir: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Changes {
    /// Work-tree root.
    pub root: PathBuf,
    /// `None` = detached HEAD.
    pub branch: Option<String>,
    pub files: Vec<ChangedFile>,
}

#[derive(Debug, thiserror::Error)]
pub enum ChangesError {
    #[error("{command} failed: {stderr}")]
    Git { command: String, stderr: String },
    #[error("the repository at the panel's root changed or is gone")]
    RootChanged,
    #[error("conflicted files cannot be discarded; resolve them first")]
    Conflicted,
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("git timed out")]
    Timeout,
}

impl From<git::GitError> for ChangesError {
    fn from(e: git::GitError) -> Self {
        match e {
            git::GitError::Timeout => ChangesError::Timeout,
            git::GitError::Spawn(e) => ChangesError::Io(e),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiffSide {
    Staged,
    Unstaged,
    Untracked,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FileDiff {
    /// Unified diff text (empty for binary files and for mode/EOL-only changes git prints no
    /// hunks for).
    pub text: String,
    pub binary: bool,
    /// Cut at 2 MiB / 20 000 lines.
    pub truncated: bool,
    pub added: u32,
    pub removed: u32,
}

// ---------------------------------------------------------------------------------------------
// Listing

/// Changes of the work tree containing `cwd`; `None` when `cwd` is not inside a work tree.
pub fn list(cwd: &Path) -> Result<Option<Changes>, ChangesError> {
    let out = git::run_bytes(
        git::command(cwd, &["rev-parse", "--show-toplevel"], true),
        READ_TIMEOUT,
        64 * 1024,
    )?;
    if !out.ok() {
        let msg = out.message();
        // 128 = "not a git repository" / "this operation must be run in a work tree" (bare repo,
        // inside `.git`).
        if out.code == Some(128)
            && (msg.contains("not a git repository") || msg.contains("work tree"))
        {
            return Ok(None);
        }
        return Err(failed("git rev-parse", &out));
    }
    let top = String::from_utf8_lossy(&out.stdout);
    let root = native(top.trim_end_matches(['\n', '\r']));
    let status = git::run_bytes(
        git::command(
            &root,
            &["status", "--porcelain=v2", "-z", "--branch", "-unormal"],
            true,
        ),
        SCAN_TIMEOUT,
        64 * 1024 * 1024,
    )?;
    if !status.ok() {
        return Err(failed("git status", &status));
    }
    let (branch, files) = parse_status(&status.stdout);
    Ok(Some(Changes {
        root,
        branch,
        files,
    }))
}

fn failed(command: &str, out: &GitBytes) -> ChangesError {
    ChangesError::Git {
        command: command.to_owned(),
        stderr: out.message(),
    }
}

fn os_string(bytes: &[u8]) -> OsString {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        OsStr::from_bytes(bytes).to_owned()
    }
    #[cfg(not(unix))]
    {
        OsString::from(String::from_utf8_lossy(bytes).into_owned())
    }
}

fn change_of(c: u8) -> Option<Change> {
    match c {
        b'M' => Some(Change::Modified),
        b'A' => Some(Change::Added),
        b'D' => Some(Change::Deleted),
        b'R' => Some(Change::Renamed),
        b'C' => Some(Change::Copied),
        b'T' => Some(Change::TypeChanged),
        _ => None,
    }
}

fn file_from(path: &[u8]) -> ChangedFile {
    let os_path = os_string(path);
    ChangedFile {
        path: os_path.to_string_lossy().into_owned(),
        is_dir: path.last() == Some(&b'/'),
        os_path,
        orig_path: None,
        staged: None,
        unstaged: None,
        untracked: false,
        conflicted: false,
    }
}

/// Parses `status --porcelain=v2 -z --branch` output: `(branch, files)`. Malformed records are
/// skipped with a log line.
fn parse_status(bytes: &[u8]) -> (Option<String>, Vec<ChangedFile>) {
    let mut branch = None;
    let mut files = Vec::new();
    let mut records = bytes.split(|&b| b == 0).filter(|r| !r.is_empty());
    while let Some(rec) = records.next() {
        let Some(&kind) = rec.first() else { continue };
        // Fields are space separated; the path is the last one and may itself contain spaces.
        let fields = match kind {
            b'#' => {
                if let Some(head) = rec.strip_prefix(b"# branch.head ") {
                    let head = String::from_utf8_lossy(head).into_owned();
                    branch = (head != "(detached)").then_some(head);
                }
                continue;
            }
            b'1' => 9,
            b'2' => 10,
            b'u' => 11,
            b'?' => {
                match rec.strip_prefix(b"? ") {
                    Some(path) if !path.is_empty() => {
                        let mut f = file_from(path);
                        f.untracked = true;
                        files.push(f);
                    }
                    _ => log::warn!("changes: malformed untracked record"),
                }
                continue;
            }
            // `!` ignored entries are not requested; anything else is unknown.
            _ => continue,
        };
        let parts: Vec<&[u8]> = rec.splitn(fields, |&b| b == b' ').collect();
        if parts.len() != fields || parts[1].len() != 2 {
            log::warn!("changes: malformed status record skipped");
            if kind == b'2' {
                records.next();
            }
            continue;
        }
        let xy = parts[1];
        let mut f = file_from(parts[fields - 1]);
        match kind {
            b'u' => f.conflicted = true,
            _ => {
                f.staged = change_of(xy[0]);
                f.unstaged = change_of(xy[1]);
            }
        }
        if kind == b'2' {
            f.orig_path = records
                .next()
                .map(|o| String::from_utf8_lossy(o).into_owned());
        }
        files.push(f);
    }
    (branch, files)
}

// ---------------------------------------------------------------------------------------------
// Diffs

/// Unified diff of `file` for one side. See [`FileDiff`] for the caps.
pub fn diff(root: &Path, file: &ChangedFile, side: DiffSide) -> Result<FileDiff, ChangesError> {
    check_operand(&file.os_path)?;
    match side {
        DiffSide::Untracked => synthesize_untracked(root, file),
        DiffSide::Staged | DiffSide::Unstaged => {
            let mut args: Vec<OsString> = DIFF_FLAGS.iter().map(OsString::from).collect();
            if side == DiffSide::Staged {
                args.push("--cached".into());
            }
            args.push("--".into());
            args.push(file.os_path.clone());
            if side == DiffSide::Staged
                && let Some(orig) = &file.orig_path
            {
                // Both names, so rename detection pairs them.
                args.insert(args.len() - 2, "-M".into());
                args.push(orig.into());
            }
            let out = git::run_bytes(
                literal(git::command(root, &args, true)),
                SCAN_TIMEOUT,
                (MAX_DIFF_BYTES + 1) as u64,
            )?;
            if !out.ok() {
                return Err(failed("git diff", &out));
            }
            Ok(finish_diff(&out.stdout))
        }
    }
}

fn literal(mut cmd: std::process::Command) -> std::process::Command {
    cmd.env("GIT_LITERAL_PATHSPECS", "1");
    cmd
}

/// Turns raw `git diff` output into a [`FileDiff`] (binary detection, caps, +/- counts).
fn finish_diff(stdout: &[u8]) -> FileDiff {
    let over_bytes = stdout.len() > MAX_DIFF_BYTES;
    let kept = &stdout[..stdout.len().min(MAX_DIFF_BYTES)];
    let text = String::from_utf8_lossy(kept);
    let mut out = String::new();
    let mut truncated = over_bytes;
    // A cut in the middle of a line leaves a partial last line: drop it.
    let complete: &str = if over_bytes {
        text.rfind('\n').map_or("", |i| &text[..=i])
    } else {
        &text
    };
    let (mut added, mut removed) = (0u32, 0u32);
    let (mut in_hunk, mut cols) = (false, 1usize);
    let mut binary = false;
    for (lines, line) in complete.split_inclusive('\n').enumerate() {
        if lines >= MAX_DIFF_LINES {
            truncated = true;
            break;
        }
        if line.starts_with("diff ") {
            in_hunk = false;
        } else if line.starts_with("@@") {
            in_hunk = true;
            cols = line.bytes().take_while(|&b| b == b'@').count() - 1;
        } else if !in_hunk
            && (line.starts_with("Binary files ") || line.starts_with("GIT binary patch"))
        {
            binary = true;
        } else if in_hunk {
            let prefix = &line.as_bytes()[..line.len().min(cols)];
            if prefix.contains(&b'+') {
                added += 1;
            }
            if prefix.contains(&b'-') {
                removed += 1;
            }
        }
        out.push_str(line);
    }
    if binary {
        return FileDiff {
            text: String::new(),
            binary: true,
            ..FileDiff::default()
        };
    }
    FileDiff {
        text: out,
        binary,
        truncated,
        added,
        removed,
    }
}

/// Diff of an untracked file (or every file of an untracked directory) against `/dev/null`,
/// synthesized here: `git diff --no-index` exits 1 on differences and cannot honour the caps.
fn synthesize_untracked(root: &Path, file: &ChangedFile) -> Result<FileDiff, ChangesError> {
    if !file.is_dir {
        let mut diff = FileDiff::default();
        let mut text = String::new();
        let mut budget = Budget::default();
        let binary = synth_file(root, &file.os_path, &mut text, &mut budget, &mut diff)?;
        if binary {
            return Ok(FileDiff {
                binary: true,
                ..FileDiff::default()
            });
        }
        diff.text = text;
        diff.truncated = budget.truncated;
        return Ok(diff);
    }
    // Directory: list its untracked, non-ignored files.
    let args = [
        OsString::from("ls-files"),
        "--others".into(),
        "--exclude-standard".into(),
        "-z".into(),
        "--".into(),
        file.os_path.clone(),
    ];
    let listed = git::run_bytes(
        literal(git::command(root, &args, true)),
        SCAN_TIMEOUT,
        16 * 1024 * 1024,
    )?;
    if !listed.ok() {
        return Err(failed("git ls-files", &listed));
    }
    let mut diff = FileDiff::default();
    let mut text = String::new();
    let mut budget = Budget::default();
    for rel in listed.stdout.split(|&b| b == 0).filter(|r| !r.is_empty()) {
        if budget.exhausted() {
            budget.truncated = true;
            break;
        }
        let rel = os_string(rel);
        let mark = text.len();
        let binary = synth_file(root, &rel, &mut text, &mut budget, &mut diff)?;
        if binary {
            text.truncate(mark);
            let name = rel.to_string_lossy();
            let header = format!(
                "diff --git a/{name} b/{name}\nnew file mode 100644\nBinary files /dev/null and b/{name} differ\n"
            );
            if budget.bytes + header.len() > MAX_DIFF_BYTES {
                budget.truncated = true;
                break;
            }
            text.push_str(&header);
            budget.bytes += header.len();
            budget.lines += 3;
        }
    }
    diff.text = text;
    diff.truncated = budget.truncated;
    Ok(diff)
}

#[derive(Default)]
struct Budget {
    bytes: usize,
    lines: usize,
    truncated: bool,
}

impl Budget {
    fn exhausted(&self) -> bool {
        self.bytes >= MAX_DIFF_BYTES || self.lines >= MAX_DIFF_LINES
    }
}

/// Appends the new-file diff of `rel` to `text`, counting added lines into `diff`. Returns `true`
/// for a binary file (nothing appended then).
fn synth_file(
    root: &Path,
    rel: &OsStr,
    text: &mut String,
    budget: &mut Budget,
    diff: &mut FileDiff,
) -> Result<bool, ChangesError> {
    let abs = root.join(rel);
    let meta = std::fs::symlink_metadata(&abs)?;
    let (mode, content): (&str, Vec<u8>) = if meta.file_type().is_symlink() {
        let target = std::fs::read_link(&abs)?;
        ("120000", target.to_string_lossy().into_owned().into_bytes())
    } else if meta.is_file() {
        let mut buf = Vec::new();
        std::fs::File::open(&abs)?
            .take(MAX_DIFF_BYTES as u64 + 1)
            .read_to_end(&mut buf)?;
        (
            if is_executable(&meta) {
                "100755"
            } else {
                "100644"
            },
            buf,
        )
    } else {
        // Sockets, FIFOs, vanished entries: nothing to show.
        return Ok(false);
    };
    if content[..content.len().min(BINARY_SNIFF)].contains(&0) {
        return Ok(true);
    }
    let mut content = content;
    let mut cut = false;
    if content.len() > MAX_DIFF_BYTES {
        content.truncate(MAX_DIFF_BYTES);
        cut = true;
    }
    let body = String::from_utf8_lossy(&content);
    let mut body: &str = &body;
    if cut {
        body = body.rfind('\n').map_or("", |i| &body[..=i]);
    }
    let mut lines: Vec<&str> = body.split_inclusive('\n').collect();
    let name = rel.to_string_lossy();
    let name = name.replace('\\', "/");
    // Whatever this entry may still add: the shared byte budget, complete lines only.
    let allowance = MAX_DIFF_BYTES.saturating_sub(budget.bytes);
    let overhead = 2 * name.len() + 160 + lines.len().to_string().len() * 2;
    let mut room = MAX_DIFF_LINES.saturating_sub(budget.lines + 5);
    let mut used = overhead;
    let fits = lines
        .iter()
        .take(room)
        .take_while(|l| {
            used += l.len() + 1;
            used <= allowance
        })
        .count();
    room = room.min(fits);
    if lines.len() > room {
        lines.truncate(room);
        cut = true;
    }
    if cut {
        budget.truncated = true;
    }
    let mut chunk = format!("diff --git a/{name} b/{name}\nnew file mode {mode}\n");
    if !lines.is_empty() {
        let n = lines.len();
        let span = if n == 1 {
            "1".to_owned()
        } else {
            format!("1,{n}")
        };
        chunk.push_str(&format!(
            "--- /dev/null\n+++ b/{name}\n@@ -0,0 +{span} @@\n"
        ));
        for (i, l) in lines.iter().enumerate() {
            chunk.push('+');
            chunk.push_str(l);
            if i + 1 == n && !l.ends_with('\n') {
                chunk.push_str("\n\\ No newline at end of file\n");
            }
        }
        diff.added = diff.added.saturating_add(n as u32);
    }
    budget.bytes += chunk.len();
    budget.lines += chunk.matches('\n').count();
    text.push_str(&chunk);
    Ok(false)
}

#[cfg(unix)]
fn is_executable(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_: &std::fs::Metadata) -> bool {
    false
}

/// Staged + unstaged changes as one unified diff (what "Copy diff" puts on the clipboard).
/// Untracked files are not part of it. Output beyond 8 MiB is cut at a line end and a final
/// `# diff truncated` line says so.
pub fn patch(root: &Path) -> Result<String, ChangesError> {
    let diff_with = |extra: &[&str]| -> Result<Vec<u8>, ChangesError> {
        let mut args: Vec<OsString> = DIFF_FLAGS.iter().map(OsString::from).collect();
        args.extend(extra.iter().map(OsString::from));
        let out = git::run_bytes(
            literal(git::command(root, &args, true)),
            SCAN_TIMEOUT,
            (MAX_PATCH_BYTES + 1) as u64,
        )?;
        if out.ok() {
            Ok(out.stdout)
        } else {
            Err(failed("git diff", &out))
        }
    };
    let bytes = if has_head(root)? {
        diff_with(&["HEAD"])?
    } else {
        // Unborn branch: everything staged is new; compare the index with the empty tree.
        let mut staged = diff_with(&["--cached"])?;
        staged.extend(diff_with(&[])?);
        staged
    };
    let mut text = String::from_utf8_lossy(&bytes[..bytes.len().min(MAX_PATCH_BYTES)]).into_owned();
    if bytes.len() > MAX_PATCH_BYTES {
        let keep = text.rfind('\n').map_or(0, |i| i + 1);
        text.truncate(keep);
        text.push_str("# diff truncated at 8 MiB\n");
    }
    Ok(text)
}

// ---------------------------------------------------------------------------------------------
// Writes

/// `Ok` when `root` is still the top level of a work tree; `RootChanged` otherwise.
fn verify_root(root: &Path) -> Result<(), ChangesError> {
    let out = git::run_bytes(
        git::command(root, &["rev-parse", "--show-toplevel"], true),
        READ_TIMEOUT,
        64 * 1024,
    )?;
    if !out.ok() {
        return Err(ChangesError::RootChanged);
    }
    let top = native(String::from_utf8_lossy(&out.stdout).trim_end_matches(['\n', '\r']));
    // Compare by identity, not spelling: git prints the long, `/`-separated form, while the
    // caller's root may be a Windows 8.3 short path, differently cased, or reached via a symlink.
    match (std::fs::canonicalize(&top), std::fs::canonicalize(root)) {
        (Ok(a), Ok(b)) if a == b => Ok(()),
        _ => Err(ChangesError::RootChanged),
    }
}

fn has_head(root: &Path) -> Result<bool, ChangesError> {
    let out = git::run_bytes(
        git::command(root, &["rev-parse", "--verify", "--quiet", "HEAD"], true),
        READ_TIMEOUT,
        64 * 1024,
    )?;
    Ok(out.ok())
}

/// Operands must be plain relative paths inside the work tree.
fn check_operand(path: &OsStr) -> Result<(), ChangesError> {
    let p = Path::new(path);
    let bad = path.is_empty()
        || p.is_absolute()
        || p.components().any(|c| {
            matches!(
                c,
                Component::ParentDir | Component::Prefix(_) | Component::RootDir
            )
        });
    if bad {
        return Err(ChangesError::Git {
            command: "git".into(),
            stderr: format!("not a path inside the repository: {}", p.display()),
        });
    }
    Ok(())
}

/// Runs `git <verb…> -- <operands>` in batches that fit a command line; the first failure wins.
fn run_batched(
    root: &Path,
    verb: &[&str],
    operands: &[&OsStr],
    timeout: Duration,
) -> Result<(), ChangesError> {
    let mut start = 0;
    while start < operands.len() {
        let mut end = start;
        let mut bytes = 0;
        while end < operands.len()
            && (end == start || bytes + operands[end].len() < MAX_OPERAND_BYTES)
        {
            bytes += operands[end].len() + 1;
            end += 1;
        }
        let mut args: Vec<&OsStr> = verb.iter().map(OsStr::new).collect();
        args.push(OsStr::new("--"));
        args.extend_from_slice(&operands[start..end]);
        let out = git::run_bytes(literal(git::command(root, &args, false)), timeout, 1 << 20)?;
        if !out.ok() {
            return Err(failed(&format!("git {}", verb[0]), &out));
        }
        start = end;
    }
    Ok(())
}

/// Collects the unique operands of `files`, validating each.
fn operands<'a>(
    files: &[&'a ChangedFile],
    extra: impl Fn(&'a ChangedFile) -> Option<&'a OsStr>,
) -> Result<Vec<&'a OsStr>, ChangesError> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for f in files {
        for p in std::iter::once(f.os_path.as_os_str()).chain(extra(f)) {
            check_operand(p)?;
            if seen.insert(p) {
                out.push(p);
            }
        }
    }
    Ok(out)
}

/// `git add -- <paths>` (also marks conflicted files resolved and stages deletions).
pub fn stage(root: &Path, files: &[&ChangedFile]) -> Result<(), ChangesError> {
    let ops = operands(files, |_| None)?;
    if ops.is_empty() {
        return Ok(());
    }
    verify_root(root)?;
    run_batched(root, &["add"], &ops, WRITE_TIMEOUT)
}

/// Removes `files` from the index: `restore --staged`, or `rm --cached` on an unborn branch.
/// A staged rename is unstaged together with its source.
pub fn unstage(root: &Path, files: &[&ChangedFile]) -> Result<(), ChangesError> {
    let ops = operands(files, |f| {
        (f.staged == Some(Change::Renamed))
            .then(|| f.orig_path.as_deref().map(OsStr::new))
            .flatten()
    })?;
    if ops.is_empty() {
        return Ok(());
    }
    verify_root(root)?;
    if has_head(root)? {
        run_batched(root, &["restore", "--staged"], &ops, WRITE_TIMEOUT)
    } else {
        run_batched(root, &["rm", "--cached", "-q", "-r"], &ops, WRITE_TIMEOUT)
    }
}

/// Throws away unstaged changes of tracked files (`git restore --`). Refuses conflicted files
/// ([`ChangesError::Conflicted`]); files without an unstaged change, untracked files and
/// intent-to-add entries (whose restore would empty the file) are left alone.
pub fn discard(root: &Path, files: &[&ChangedFile]) -> Result<(), ChangesError> {
    if files.iter().any(|f| f.conflicted) {
        return Err(ChangesError::Conflicted);
    }
    let eligible: Vec<&ChangedFile> = files
        .iter()
        .copied()
        .filter(|f| !f.untracked && !matches!(f.unstaged, None | Some(Change::Added)))
        .collect();
    let ops = operands(&eligible, |_| None)?;
    if ops.is_empty() {
        return Ok(());
    }
    verify_root(root)?;
    run_batched(root, &["restore"], &ops, WRITE_TIMEOUT)
}

/// Deletes untracked files / directories. Each path is first run through `git clean -n`, which
/// must list exactly that path (below it, for a directory) and nothing else; only then
/// `git clean -f[d] -- <path>` runs. Never `-x`, never without a pathspec.
pub fn delete_untracked(root: &Path, files: &[&ChangedFile]) -> Result<(), ChangesError> {
    for f in files {
        check_operand(&f.os_path)?;
        if !f.untracked {
            return Err(ChangesError::Git {
                command: "git clean".into(),
                stderr: format!("{} is not untracked", f.path),
            });
        }
    }
    if files.is_empty() {
        return Ok(());
    }
    verify_root(root)?;
    for f in files {
        let dry: &[&str] = if f.is_dir {
            &["clean", "-n", "-d", "--"]
        } else {
            &["clean", "-n", "--"]
        };
        let mut args: Vec<&OsStr> = dry.iter().map(OsStr::new).collect();
        args.push(&f.os_path);
        let out = git::run_bytes(
            literal(git::command(root, &args, false)),
            WRITE_TIMEOUT,
            4 << 20,
        )?;
        if !out.ok() {
            return Err(failed("git clean", &out));
        }
        let listed = String::from_utf8_lossy(&out.stdout);
        let mut lines = listed.lines().filter(|l| !l.is_empty()).peekable();
        if lines.peek().is_none() {
            if std::fs::symlink_metadata(root.join(&f.os_path)).is_err() {
                // Already gone.
                continue;
            }
            // Still there but git would not remove it (nested repository, …).
            return Err(ChangesError::Git {
                command: "git clean".into(),
                stderr: format!("git would not remove {}", f.path),
            });
        }
        let want = f.os_path.to_string_lossy().replace('\\', "/");
        for line in lines {
            let ok = line
                .strip_prefix("Would remove ")
                .map(unquote_c)
                .is_some_and(|p| p == want || (f.is_dir && p.starts_with(&want)));
            if !ok {
                return Err(ChangesError::Git {
                    command: "git clean".into(),
                    stderr: format!("refusing to delete {}: dry run said `{line}`", f.path),
                });
            }
        }
        let verb: &[&str] = if f.is_dir {
            &["clean", "-f", "-d"]
        } else {
            &["clean", "-f"]
        };
        run_batched(root, verb, &[f.os_path.as_os_str()], WRITE_TIMEOUT)?;
    }
    Ok(())
}

/// Undoes git's C-style path quoting (`"a\tb"`, `"\303\251"`); unquoted text is returned as is.
fn unquote_c(s: &str) -> String {
    let Some(inner) = s.strip_prefix('"').and_then(|s| s.strip_suffix('"')) else {
        return s.to_owned();
    };
    let b = inner.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'\\' || i + 1 == b.len() {
            out.push(b[i]);
            i += 1;
            continue;
        }
        i += 1;
        match b[i] {
            b'a' => out.push(7),
            b'b' => out.push(8),
            b't' => out.push(b'\t'),
            b'n' => out.push(b'\n'),
            b'v' => out.push(11),
            b'f' => out.push(12),
            b'r' => out.push(b'\r'),
            d @ b'0'..=b'3'
                if i + 2 < b.len() && b[i + 1].is_ascii_digit() && b[i + 2].is_ascii_digit() =>
            {
                let v = (d - b'0') * 64 + (b[i + 1] - b'0') * 8 + (b[i + 2] - b'0');
                out.push(v);
                i += 2;
            }
            other => out.push(other),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests;
