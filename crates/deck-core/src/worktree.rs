//! Git worktree sessions: detecting linked worktrees, creating them through `omp worktree add`,
//! inspecting their state and removing them safely.
//!
//! Everything here blocks (git / omp child processes); callers run it on workers. Process runs go
//! through the bounded, tree-killing runner of [`crate::usage`]. OmpDeck never writes under
//! `~/.omp`: omp itself creates the worktree directory below its managed base.

use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use crate::git::{self, GitOut, native};
use crate::omp::Omp;
use crate::usage::{UsageError, run_bounded};

/// Directory levels `resolve` climbs looking for `.git`.
const MAX_WALK: usize = 64;
const GIT_READ_TIMEOUT: Duration = Duration::from_secs(15);
const GIT_WRITE_TIMEOUT: Duration = Duration::from_secs(120);
const OMP_TIMEOUT: Duration = Duration::from_secs(120);
/// Longest slug in bytes (slugs are ASCII).
const SLUG_MAX: usize = 60;
/// `-2`, `-3`, … suffixes tried by [`default_location`] before giving up on uniqueness.
const MAX_SUFFIX: u32 = 10_000;
/// omp overlay forcing a plain checkout (see [`create`]).
const OVERLAY_YAML: &str = "worktree:\n  clone: false\n";

/// A linked (non-primary) git worktree.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Worktree {
    /// Worktree root (the directory containing the `.git` file).
    pub path: PathBuf,
    /// Root of the main checkout the worktree belongs to.
    pub primary: PathBuf,
    /// `None` = detached HEAD.
    pub branch: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum WorktreeError {
    #[error("not a git repository")]
    NotARepo,
    #[error("{command} failed: {stderr}")]
    Git { command: String, stderr: String },
    #[error("omp worktree failed: {stderr}")]
    Omp { stderr: String },
    #[error("worktree cannot be removed safely: {}", describe(.0))]
    Unsafe(Vec<Unsafe>),
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("timed out")]
    Timeout,
}

/// Why a worktree must not be removed automatically.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Unsafe {
    /// Staged, unstaged or untracked changes (ignored files do not count).
    Dirty,
    /// Commits that are not in the base.
    NotMerged { ahead: u32 },
    /// Detached HEAD whose commit is not in the base (it would become unreachable).
    Detached,
    /// The worktree directory no longer exists.
    Missing,
}

fn describe(reasons: &[Unsafe]) -> String {
    let parts: Vec<String> = reasons
        .iter()
        .map(|r| match r {
            Unsafe::Dirty => "uncommitted or untracked changes".to_owned(),
            Unsafe::NotMerged { ahead } => format!("{ahead} commit(s) not merged into the base"),
            Unsafe::Detached => "detached HEAD with commits not in the base".to_owned(),
            Unsafe::Missing => "the worktree folder is missing".to_owned(),
        })
        .collect();
    parts.join("; ")
}

// ---------------------------------------------------------------------------------------------
// Filesystem-only detection

/// Lexical normalisation (`.`/`..` folded, no symlink resolution, no I/O).
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// A path written by git: forward slashes on Windows (`C:/x/y`), relative to `base` when relative.
fn git_path(raw: &str, base: &Path) -> PathBuf {
    let raw = raw.trim();
    #[cfg(windows)]
    let raw = raw.replace('/', "\\");
    let p = PathBuf::from(&raw);
    normalize(&if p.is_absolute() { p } else { base.join(p) })
}

/// Linked worktree containing `dir`, or `None` for the primary checkout, non-repositories,
/// submodules, bare layouts and unreadable metadata. Reads a few small files, spawns nothing.
pub fn resolve(dir: &Path) -> Option<Worktree> {
    let start = normalize(dir);
    let mut cur: &Path = &start;
    for _ in 0..MAX_WALK {
        let dot_git = cur.join(".git");
        if let Ok(meta) = std::fs::symlink_metadata(&dot_git) {
            if meta.is_dir() {
                return None;
            }
            return linked_from_file(cur, &dot_git);
        }
        cur = cur.parent()?;
    }
    None
}

fn linked_from_file(root: &Path, dot_git: &Path) -> Option<Worktree> {
    // `.git` files are tiny: `gitdir: <path>`. Bound the read anyway.
    use std::io::Read;
    let mut text = String::new();
    std::fs::File::open(dot_git)
        .ok()?
        .take(4096)
        .read_to_string(&mut text)
        .ok()?;
    let gitdir = git_path(text.lines().next()?.strip_prefix("gitdir:")?, root);
    let common = git_path(
        std::fs::read_to_string(gitdir.join("commondir"))
            .ok()?
            .lines()
            .next()?,
        &gitdir,
    );
    if common.file_name() != Some(OsStr::new(".git")) {
        return None;
    }
    let primary = common.parent()?.to_path_buf();
    let head = std::fs::read_to_string(gitdir.join("HEAD")).ok()?;
    let branch = head
        .lines()
        .next()
        .and_then(|l| l.strip_prefix("ref: refs/heads/"))
        .map(|b| b.trim().to_owned());
    Some(Worktree {
        path: root.to_path_buf(),
        primary,
        branch,
    })
}

// ---------------------------------------------------------------------------------------------
// Locations

/// omp's `getWorktreesDir`: `OMP_WORKTREE_DIR` > the `worktree.base` `setting` > `<home>/.omp/wt`.
/// Values expand a leading `~` and must be absolute; relative ones are ignored.
pub fn managed_base(setting: Option<&str>) -> PathBuf {
    managed_base_from(
        std::env::var("OMP_WORKTREE_DIR").ok().as_deref(),
        setting,
        &crate::platform::home_or_root(),
    )
}

fn managed_base_from(env: Option<&str>, setting: Option<&str>, home: &Path) -> PathBuf {
    let expand = |v: &str| -> Option<PathBuf> {
        let v = v.trim();
        let p = if v == "~" {
            home.to_path_buf()
        } else if let Some(rest) = v.strip_prefix("~/").or_else(|| v.strip_prefix("~\\")) {
            home.join(rest)
        } else {
            PathBuf::from(v)
        };
        (!v.is_empty() && p.is_absolute()).then(|| normalize(&p))
    };
    env.and_then(expand)
        .or_else(|| setting.and_then(expand))
        .unwrap_or_else(|| home.join(".omp").join("wt"))
}

/// Path-safe folder-name fragment of a branch: `[A-Za-z0-9._-]` kept, anything else → `-`,
/// runs of `-` collapsed, leading/trailing `-` and `.` trimmed, ≤ 60 bytes, never empty.
pub fn slug(branch: &str) -> String {
    let mut out = String::with_capacity(branch.len().min(SLUG_MAX + 1));
    for c in branch.chars() {
        let c = if c.is_ascii_alphanumeric() || c == '.' || c == '_' {
            c
        } else {
            '-'
        };
        if c == '-' && out.ends_with('-') {
            continue;
        }
        out.push(c);
    }
    let trimmed = |s: &str| s.trim_matches(|c| c == '-' || c == '.').to_owned();
    let mut s = trimmed(&out);
    // ASCII only, so byte truncation is on a char boundary.
    s.truncate(SLUG_MAX);
    let s = trimmed(&s);
    if s.is_empty() {
        "worktree".to_owned()
    } else {
        s
    }
}

/// `<base>/<slug(branch)>-<slug(primary name)>`, then `-2`, `-3`, … while the path exists.
pub fn default_location(base: &Path, primary: &Path, branch: &str) -> PathBuf {
    let repo = primary
        .file_name()
        .map_or_else(|| "repo".to_owned(), |n| slug(&n.to_string_lossy()));
    let name = format!("{}-{repo}", slug(branch));
    let first = base.join(&name);
    if !first.exists() {
        return first;
    }
    for n in 2..=MAX_SUFFIX {
        let candidate = base.join(format!("{name}-{n}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    base.join(format!("{name}-{MAX_SUFFIX}"))
}

// ---------------------------------------------------------------------------------------------
// git runner

impl From<git::GitError> for WorktreeError {
    fn from(e: git::GitError) -> Self {
        match e {
            git::GitError::Timeout => WorktreeError::Timeout,
            git::GitError::Spawn(e) => WorktreeError::Io(e),
        }
    }
}

fn usage_err(e: UsageError) -> WorktreeError {
    match e {
        UsageError::Timeout(_) => WorktreeError::Timeout,
        UsageError::Spawn(e) => WorktreeError::Io(e),
        other => WorktreeError::Git {
            command: "git".into(),
            stderr: other.to_string(),
        },
    }
}

/// Runs git and returns its output whatever the exit code (callers interpret it).
fn git_raw(
    cwd: &Path,
    args: &[&str],
    read_only: bool,
    timeout: Duration,
) -> Result<GitOut, WorktreeError> {
    Ok(git::raw(cwd, args, read_only, timeout)?)
}

fn failed(args: &[&str], out: &GitOut) -> WorktreeError {
    WorktreeError::Git {
        command: format!("git {}", args.first().copied().unwrap_or("")),
        stderr: out.message(),
    }
}

/// Runs git, requiring exit 0; returns trimmed stdout.
fn git_ok(
    cwd: &Path,
    args: &[&str],
    read_only: bool,
    timeout: Duration,
) -> Result<String, WorktreeError> {
    let out = git_raw(cwd, args, read_only, timeout)?;
    if out.ok() {
        Ok(out.stdout.trim().to_owned())
    } else {
        Err(failed(args, &out))
    }
}

// ---------------------------------------------------------------------------------------------
// Repository info

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RepoInfo {
    /// Toplevel of the queried directory's working tree.
    pub root: PathBuf,
    /// Main checkout root (`root` itself for a primary checkout).
    pub primary: PathBuf,
    pub current_branch: Option<String>,
    /// Local branches first, then remote-tracking `origin/x`; no `HEAD` aliases.
    pub branches: Vec<String>,
    pub has_commits: bool,
}

pub fn repo_info(dir: &Path) -> Result<RepoInfo, WorktreeError> {
    let top = git_raw(
        dir,
        &["rev-parse", "--show-toplevel"],
        true,
        GIT_READ_TIMEOUT,
    )?;
    let top = top.stdout.trim();
    if top.is_empty() || top.contains('\n') {
        return Err(WorktreeError::NotARepo);
    }
    let root = native(top);
    let primary = resolve(&root).map_or_else(|| root.clone(), |w| w.primary);
    let current_branch = branch_of(&root);
    let has_commits = git_raw(
        &root,
        &["rev-parse", "--verify", "-q", "HEAD"],
        true,
        GIT_READ_TIMEOUT,
    )?
    .ok();
    let refs = git_ok(
        &root,
        &[
            "for-each-ref",
            "--format=%(refname)",
            "refs/heads",
            "refs/remotes",
        ],
        true,
        GIT_READ_TIMEOUT,
    )?;
    let mut local = Vec::new();
    let mut remote = Vec::new();
    for r in refs.lines() {
        if let Some(b) = r.strip_prefix("refs/heads/") {
            local.push(b.to_owned());
        } else if let Some(b) = r.strip_prefix("refs/remotes/")
            && b != "HEAD"
            && !b.ends_with("/HEAD")
        {
            remote.push(b.to_owned());
        }
    }
    local.extend(remote);
    Ok(RepoInfo {
        root,
        primary,
        current_branch,
        branches: local,
        has_commits,
    })
}

/// Checked-out branch of the working tree at `dir` (also on an unborn branch); `None` = detached.
fn branch_of(dir: &Path) -> Option<String> {
    let out = git_raw(
        dir,
        &["symbolic-ref", "-q", "--short", "HEAD"],
        true,
        GIT_READ_TIMEOUT,
    )
    .ok()?;
    let b = out.stdout.trim();
    (out.ok() && !b.is_empty()).then(|| b.to_owned())
}

/// `Ok` when `name` is a valid, not yet existing local branch name for `repo`.
pub fn check_branch_name(repo: &Path, name: &str) -> Result<(), WorktreeError> {
    let args = ["check-ref-format", "--branch", name];
    let out = git_raw(repo, &args, true, GIT_READ_TIMEOUT)?;
    // `--branch` also expands `@{-1}`-style shorthands; those are not names.
    if !out.ok() || out.stdout.trim() != name {
        return Err(WorktreeError::Git {
            command: "git check-ref-format".into(),
            stderr: format!("'{name}' is not a valid branch name"),
        });
    }
    let full = format!("refs/heads/{name}");
    let exists = git_raw(
        repo,
        &["show-ref", "--verify", "--quiet", &full],
        true,
        GIT_READ_TIMEOUT,
    )?;
    if exists.ok() {
        return Err(WorktreeError::Git {
            command: "git show-ref".into(),
            stderr: format!("branch '{name}' already exists"),
        });
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Create

/// OmpDeck-owned omp settings overlay (never `~/.omp`): `<data dir>/omp-deck/worktree-overlay.yml`.
/// The data dir follows the environment `omp` is configured with ([`Omp::with_env`]) before the
/// process environment, so sandboxed runs stay sandboxed.
fn overlay_path(omp: &Omp) -> PathBuf {
    let abs = |v: &std::ffi::OsString| Some(PathBuf::from(v)).filter(|p| p.is_absolute());
    let data = if cfg!(windows) {
        omp.env_value("APPDATA").and_then(abs)
    } else if cfg!(target_os = "macos") {
        omp.env_value("HOME")
            .and_then(abs)
            .map(|h| h.join("Library").join("Application Support"))
    } else {
        omp.env_value("XDG_DATA_HOME").and_then(abs).or_else(|| {
            omp.env_value("HOME")
                .and_then(abs)
                .map(|h| h.join(".local").join("share"))
        })
    };
    data.or_else(dirs::data_dir)
        .unwrap_or_else(std::env::temp_dir)
        .join("omp-deck")
        .join("worktree-overlay.yml")
}

fn ensure_overlay(omp: &Omp) -> std::io::Result<PathBuf> {
    let path = overlay_path(omp);
    if std::fs::read_to_string(&path).is_ok_and(|t| t == OVERLAY_YAML) {
        return Ok(path);
    }
    if let Some(dir) = path.parent() {
        crate::platform::create_private_dir_all(dir)?;
    }
    crate::platform::write_private(&path, OVERLAY_YAML.as_bytes())?;
    Ok(path)
}

/// `omp worktree add -q -C <repo> -b <branch> <dest> <base_ref>`: new branch `branch` from
/// `base_ref`, checked out at `dest` (absent or empty). omp reads `PI_CONFIG_FILES` as extra
/// YAML settings layers (`config/settings.ts`, entries after the user's own win); the overlay sets
/// `worktree.clone: false`, so the worktree is a clean checkout and gitignored files (`.env`,
/// `node_modules`, `target`) are not copied. Failures carry omp's stderr.
pub fn create(
    omp: &Omp,
    repo: &Path,
    branch: &str,
    base_ref: &str,
    dest: &Path,
) -> Result<(), WorktreeError> {
    check_base(base_ref)?;
    if !dest.is_absolute() {
        return Err(WorktreeError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "worktree location must be an absolute path",
        )));
    }
    let overlay = ensure_overlay(omp)?;
    // The child inherits the process env except what `Omp::with_env` overrides.
    let inherited = omp
        .env_value("PI_CONFIG_FILES")
        .cloned()
        .or_else(|| std::env::var_os("PI_CONFIG_FILES"));
    let mut files: Vec<PathBuf> = inherited
        .map(|v| std::env::split_paths(&v).collect())
        .unwrap_or_default();
    files.push(overlay);
    let joined = std::env::join_paths(files)
        .map_err(|e| WorktreeError::Io(std::io::Error::new(std::io::ErrorKind::InvalidInput, e)))?;
    let args: [&OsStr; 9] = [
        OsStr::new("worktree"),
        OsStr::new("add"),
        OsStr::new("-q"),
        OsStr::new("-C"),
        repo.as_os_str(),
        OsStr::new("-b"),
        OsStr::new(branch),
        dest.as_os_str(),
        OsStr::new(base_ref),
    ];
    let mut cmd = omp.command(&args);
    cmd.env("PI_CONFIG_FILES", joined);
    let (code, stdout, stderr) = run_bounded(cmd, OMP_TIMEOUT).map_err(usage_err)?;
    if code == Some(0) {
        return Ok(());
    }
    let text = if stderr.trim().is_empty() {
        stdout.trim()
    } else {
        stderr.trim()
    };
    Err(WorktreeError::Omp {
        stderr: text.to_owned(),
    })
}

// ---------------------------------------------------------------------------------------------
// Status / removal

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WorktreeStatus {
    /// `None` = detached.
    pub branch: Option<String>,
    /// Staged, unstaged or untracked changes; ignored files excluded.
    pub dirty: bool,
    /// HEAD is an ancestor of the base.
    pub merged: bool,
    /// Commits of HEAD not in the base.
    pub ahead: u32,
}

fn check_base(base: &str) -> Result<(), WorktreeError> {
    if base.is_empty() || base.starts_with('-') {
        return Err(WorktreeError::Git {
            command: "git rev-parse".into(),
            stderr: format!("'{base}' is not a valid base ref"),
        });
    }
    Ok(())
}

pub fn status(wt: &Worktree, base: &str) -> Result<WorktreeStatus, WorktreeError> {
    check_base(base)?;
    if !wt.path.is_dir() {
        return Err(WorktreeError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("{} does not exist", wt.path.display()),
        )));
    }
    let dir = wt.path.as_path();
    let porcelain = git_ok(
        dir,
        &["status", "--porcelain", "-unormal"],
        true,
        GIT_READ_TIMEOUT,
    )?;
    let verify = format!("{base}^{{commit}}");
    git_ok(
        dir,
        &["rev-parse", "--verify", "-q", &verify],
        true,
        GIT_READ_TIMEOUT,
    )
    .map_err(|_| WorktreeError::Git {
        command: "git rev-parse".into(),
        stderr: format!("base '{base}' does not exist"),
    })?;
    let anc = git_raw(
        dir,
        &["merge-base", "--is-ancestor", "HEAD", base],
        true,
        GIT_READ_TIMEOUT,
    )?;
    let merged = match anc.code {
        Some(0) => true,
        Some(1) => false,
        _ => return Err(failed(&["merge-base"], &anc)),
    };
    let not_in_base = format!("^{base}");
    let count = git_ok(
        dir,
        &["rev-list", "--count", "HEAD", &not_in_base],
        true,
        GIT_READ_TIMEOUT,
    )?;
    Ok(WorktreeStatus {
        branch: branch_of(dir),
        dirty: !porcelain.is_empty(),
        merged,
        ahead: count.parse().unwrap_or(0),
    })
}

/// Primary checkout's current branch, else the remote default branch (`origin/HEAD` target).
pub fn infer_base(wt: &Worktree) -> Option<String> {
    if let Some(b) = branch_of(&wt.primary) {
        return Some(b);
    }
    let out = git_raw(
        &wt.primary,
        &["symbolic-ref", "-q", "--short", "refs/remotes/origin/HEAD"],
        true,
        GIT_READ_TIMEOUT,
    )
    .ok()?;
    let b = out.stdout.trim();
    (out.ok() && !b.is_empty()).then(|| b.to_owned())
}

/// Outcome of a successful [`remove`]: the folder is gone; the branch may have been kept.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Removed {
    pub branch_deleted: bool,
    /// Why the branch was kept although deletion was requested.
    pub branch_note: Option<String>,
}

/// Deletes `refs/heads/<branch>` only if it still points at `tip` (git's old-value check).
fn delete_branch_at(primary: &Path, branch: &str, tip: &str) -> Result<(), WorktreeError> {
    let full = format!("refs/heads/{branch}");
    git_ok(
        primary,
        &["update-ref", "-d", &full, tip],
        false,
        GIT_WRITE_TIMEOUT,
    )?;
    // What `git branch -d` also does; best effort (the section may not exist).
    let section = format!("branch.{branch}");
    let _ = git_raw(
        primary,
        &["config", "--remove-section", &section],
        false,
        GIT_READ_TIMEOUT,
    );
    Ok(())
}

/// Removes a worktree that is clean and fully merged into `base`. Re-checks [`status`] first and
/// returns [`WorktreeError::Unsafe`] without touching anything otherwise. Never forces
/// (`--force`, `-D`).
///
/// With `delete_branch` the branch ref is then deleted with `git update-ref -d refs/heads/<b>
/// <tip>`, where `<tip>` is the commit verified (just before the removal) to be an ancestor of
/// `base`; the old-value check makes this `branch -d` relative to `base` (git's own `-d` compares
/// with the primary's HEAD/upstream instead) and atomic: a branch that moved meanwhile is kept
/// and explained in [`Removed::branch_note`]. A failed branch deletion is not an error: the
/// folder is already gone and callers must still prune.
pub fn remove(wt: &Worktree, base: &str, delete_branch: bool) -> Result<Removed, WorktreeError> {
    if !wt.path.is_dir() {
        return Err(WorktreeError::Unsafe(vec![Unsafe::Missing]));
    }
    let st = status(wt, base)?;
    let mut reasons = Vec::new();
    if st.dirty {
        reasons.push(Unsafe::Dirty);
    }
    if st.branch.is_none() {
        if !st.merged {
            reasons.push(Unsafe::Detached);
        }
    } else if !st.merged {
        reasons.push(Unsafe::NotMerged { ahead: st.ahead });
    }
    if !reasons.is_empty() {
        return Err(WorktreeError::Unsafe(reasons));
    }
    // Pin the verified commit: the branch is only ever deleted at exactly this tip.
    let tip = git_ok(
        &wt.path,
        &["rev-parse", "--verify", "-q", "HEAD"],
        true,
        GIT_READ_TIMEOUT,
    )?;
    let anc = git_raw(
        &wt.path,
        &["merge-base", "--is-ancestor", &tip, base],
        true,
        GIT_READ_TIMEOUT,
    )?;
    if anc.code != Some(0) {
        return Err(WorktreeError::Unsafe(vec![Unsafe::NotMerged {
            ahead: st.ahead.max(1),
        }]));
    }
    let path = wt.path.to_string_lossy();
    git_ok(
        &wt.primary,
        &["worktree", "remove", "--", &path],
        false,
        GIT_WRITE_TIMEOUT,
    )?;
    let mut done = Removed {
        branch_deleted: false,
        branch_note: None,
    };
    if delete_branch && let Some(branch) = &st.branch {
        match delete_branch_at(&wt.primary, branch, &tip) {
            Ok(()) => done.branch_deleted = true,
            Err(e) => done.branch_note = Some(format!("branch '{branch}' was kept: {e}")),
        }
    }
    Ok(done)
}

// ---------------------------------------------------------------------------------------------
// Finish and removal policy (pure; shared by every client of the gateway)

/// A running session tab as the removal classifier sees it.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TabCwd {
    pub uid: u64,
    /// Shown in the refusal message.
    pub label: String,
    /// The folder the tab was launched in (may be stale after a `/wt` move).
    pub stored: PathBuf,
    /// The folder its omp runs in now, when it could be read (the terminal breadcrumb).
    pub live: Option<PathBuf>,
}

/// The canonical form of `p`: symlinks resolved; a path that no longer exists keeps its missing
/// tail below the deepest existing ancestor. The Windows verbatim prefix is dropped.
fn canonical(p: &Path) -> PathBuf {
    let mut tail: Vec<&OsStr> = Vec::new();
    let mut cur = p;
    loop {
        if let Ok(c) = std::fs::canonicalize(cur) {
            let mut out = crate::platform::strip_verbatim(c);
            out.extend(tail.iter().rev());
            return out;
        }
        match (cur.parent(), cur.file_name()) {
            (Some(parent), Some(name)) => {
                tail.push(name);
                cur = parent;
            }
            _ => return p.to_path_buf(),
        }
    }
}

/// The path's components as comparable keys (case-folded on Windows, whose file systems are
/// case-insensitive by default).
fn keys(p: &Path) -> Vec<String> {
    p.components()
        .map(|c| {
            let s = c.as_os_str().to_string_lossy();
            if cfg!(windows) {
                s.to_lowercase()
            } else {
                s.into_owned()
            }
        })
        .collect()
}

/// The tabs to stop when the worktree goes away: those whose live folder is inside it. A tab
/// whose live folder is unknown counts when it was launched inside the worktree; one launched in
/// the worktree's repository (where `/wt` could have moved it into the worktree) cannot be
/// judged, so the error names it and removal must not proceed. Everything else is unrelated.
/// Blocking: reads the file system.
pub fn classify(wt: &Worktree, tabs: &[TabCwd]) -> Result<Vec<u64>, String> {
    let root = keys(&canonical(&wt.path));
    let primary = keys(&canonical(&wt.primary));
    let mut stop = Vec::new();
    for t in tabs {
        let hit = match &t.live {
            Some(live) => keys(&canonical(live)).starts_with(&root),
            None => {
                let stored = keys(&canonical(&t.stored));
                if stored.starts_with(&root) {
                    true
                } else if stored.starts_with(&primary) || primary.starts_with(&stored) {
                    return Err(format!(
                        "OmpDeck cannot tell where the session “{}” runs (it may already be inside the worktree); close it first",
                        t.label
                    ));
                } else {
                    false
                }
            }
        };
        if hit {
            stop.push(t.uid);
        }
    }
    Ok(stop)
}

/// `s` as a markdown code span: the fence is longer than any backtick run inside, so a value
/// can never end the span early.
fn code_span(s: &str) -> String {
    let mut longest = 0;
    let mut run = 0;
    for c in s.chars() {
        run = if c == '`' { run + 1 } else { 0 };
        longest = longest.max(run);
    }
    let fence = "`".repeat(longest + 1);
    if longest > 0 || s.starts_with(' ') || s.ends_with(' ') {
        format!("{fence} {s} {fence}")
    } else {
        format!("{fence}{s}{fence}")
    }
}

/// POSIX single-quote `s` (always quoted; embedded `'` becomes `'\''`).
pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// One line without control characters: whitespace controls (newline, tab, NEL …) become a single
/// blank, every other C0/C1 control (ESC, DEL, …) is dropped. An `ESC[201~` inside a pasted title
/// would otherwise end the bracketed paste early and run the rest as keystrokes.
pub fn sanitize_line(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_whitespace() {
            if !out.ends_with(' ') {
                out.push(' ');
            }
        } else if !c.is_control() {
            out.push(c);
        }
    }
    out.trim().to_owned()
}

/// Where the branch has to be merged so that `base` advances.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum MergeTarget {
    /// `base` is checked out in this worktree (the primary or another one): merge there.
    In(PathBuf),
    /// `base` is checked out nowhere: bring it into the branch, then fast-forward it by pushing
    /// to the local ref, which switches no checkout.
    Nowhere,
}

/// The worktree of `git worktree list --porcelain` output that has `base` checked out.
pub fn merge_target(porcelain: &str, base: &str) -> MergeTarget {
    let want = format!("refs/heads/{base}");
    let mut path: Option<&str> = None;
    for line in porcelain.lines() {
        if let Some(p) = line.strip_prefix("worktree ") {
            path = Some(p);
        } else if line.strip_prefix("branch ") == Some(want.as_str())
            && let Some(p) = path
        {
            return MergeTarget::In(PathBuf::from(p));
        }
    }
    MergeTarget::Nowhere
}

/// Finds where `base` is checked out (blocking: runs git).
pub fn find_merge_target(primary: &Path, base: &str) -> Result<MergeTarget, String> {
    let mut cmd = std::process::Command::new("git");
    cmd.args(["worktree", "list", "--porcelain"])
        .current_dir(primary)
        .stdin(std::process::Stdio::null());
    crate::platform::no_window(&mut cmd);
    let out = cmd
        .output()
        .map_err(|e| format!("could not run git: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git worktree list failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(merge_target(&String::from_utf8_lossy(&out.stdout), base))
}

/// The one-line prompt sent to the worktree's session. It is sanitized here, so a branch or path
/// with control characters can never break out of the paste.
pub fn finish_prompt(
    branch: Option<&str>,
    base: &str,
    worktree: &Path,
    target: &MergeTarget,
) -> String {
    use shell_quote as q;
    let work = match branch {
        Some(b) => format!("branch {}", code_span(b)),
        None => "this detached HEAD (create a branch for it first)".to_owned(),
    };
    let wt_dir = q(&worktree.to_string_lossy());
    let how = match target {
        MergeTarget::In(dir) => {
            let dir_s = dir.to_string_lossy();
            let merge = format!(
                "git -C {} merge {}",
                q(&dir_s),
                branch.map_or_else(|| "<that branch>".to_owned(), q)
            );
            format!(
                "then merge it into {} (checked out at {}), for example {}",
                code_span(base),
                code_span(&dir_s),
                code_span(&merge)
            )
        }
        MergeTarget::Nowhere => format!(
            "then update {base_span}, which is not checked out anywhere, without switching any checkout: first bring it into the work with {merge}, then fast-forward it with {push}",
            base_span = code_span(base),
            merge = code_span(&format!("git -C {wt_dir} merge {}", q(base))),
            push = code_span(&format!(
                "git -C {wt_dir} push . {}",
                q(&format!("HEAD:refs/heads/{base}"))
            )),
        ),
    };
    sanitize_line(&format!(
        "Finish this worktree: commit any remaining work on {work}, {how}. Follow this project's rules on committing and merging; if they do not let you do that without being asked, ask me first. Do not delete the worktree or its branch — OmpDeck removes both once everything is merged. When you are done, tell me the result."
    ))
}

/// Why `status` does not allow removing the worktree (empty = safe: clean and merged into `base`).
pub fn blockers(st: &WorktreeStatus, base: &str) -> Vec<String> {
    let mut out = Vec::new();
    if st.dirty {
        out.push("it has uncommitted or untracked changes".to_owned());
    }
    if !st.merged {
        out.push(match st.branch {
            Some(_) => format!(
                "{} commit{} not merged into {base}",
                st.ahead,
                if st.ahead == 1 { " is" } else { "s are" }
            ),
            None => format!("its detached HEAD has commits that are not in {base}"),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn branch_deletion_is_conditional_on_the_verified_tip() {
        let git = |dir: &Path, args: &[&str]| {
            let out = Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(["-c", "user.name=t", "-c", "user.email=t@t"])
                .args(args)
                .output()
                .unwrap();
            assert!(out.status.success(), "{args:?}");
            String::from_utf8_lossy(&out.stdout).trim().to_owned()
        };
        let d = tempfile::tempdir().unwrap();
        git(d.path(), &["init", "-q", "-b", "main"]);
        git(d.path(), &["commit", "-q", "--allow-empty", "-m", "one"]);
        let tip = git(d.path(), &["rev-parse", "HEAD"]);
        git(d.path(), &["branch", "topic"]);
        git(d.path(), &["commit", "-q", "--allow-empty", "-m", "two"]);

        // The branch moved after the check: it is kept.
        git(d.path(), &["update-ref", "refs/heads/topic", "HEAD"]);
        assert!(delete_branch_at(d.path(), "topic", &tip).is_err());
        assert_eq!(
            git(d.path(), &["branch", "--list", "topic"]).trim(),
            "topic"
        );

        // Unmoved: deleted.
        let now = git(d.path(), &["rev-parse", "topic"]);
        delete_branch_at(d.path(), "topic", &now).unwrap();
        assert!(git(d.path(), &["branch", "--list", "topic"]).is_empty());
    }

    #[test]
    fn slug_edge_cases() {
        assert_eq!(slug("wt/20261023-1010"), "wt-20261023-1010");
        assert_eq!(slug("Feature/Ünï code"), "Feature-n-code");
        assert_eq!(slug("..."), "worktree");
        assert_eq!(slug(".."), "worktree");
        assert_eq!(slug(""), "worktree");
        assert_eq!(slug("///"), "worktree");
        assert_eq!(slug("-a--b-"), "a-b");
        assert_eq!(slug(".hidden"), "hidden");
        assert_eq!(slug("keep_this.v1"), "keep_this.v1");
        let long = "a/".repeat(100);
        let s = slug(&long);
        assert!(s.len() <= SLUG_MAX && !s.ends_with('-'), "{s}");
        // Truncation inside a run must not leave a trailing separator.
        let edge = format!("{}/zzz", "a".repeat(SLUG_MAX - 1));
        assert_eq!(slug(&edge), "a".repeat(SLUG_MAX - 1));
    }

    #[test]
    fn managed_base_precedence() {
        let home = Path::new(if cfg!(windows) {
            r"C:\Users\u"
        } else {
            "/home/u"
        });
        let b = |env, setting| managed_base_from(env, setting, home);
        assert_eq!(b(None, None), home.join(".omp").join("wt"));
        assert_eq!(b(None, Some("~/trees")), home.join("trees"));
        assert_eq!(b(None, Some("~")), home);
        if cfg!(unix) {
            assert_eq!(b(Some("/x/env"), Some("/x/setting")), Path::new("/x/env"));
            assert_eq!(b(None, Some("/x/setting/")), Path::new("/x/setting"));
            // Relative values are ignored and resolution falls through.
            assert_eq!(b(Some("rel"), Some("/x/s")), Path::new("/x/s"));
            assert_eq!(b(Some(" "), Some("also/rel")), home.join(".omp").join("wt"));
        }
    }

    #[test]
    fn git_paths_resolve_relative_and_normalise() {
        let base = Path::new(if cfg!(windows) { "C:\\r\\wt" } else { "/r/wt" });
        let got = git_path("../.git/worktrees/wt", base);
        let want = Path::new(if cfg!(windows) { "C:\\r" } else { "/r" })
            .join(".git")
            .join("worktrees")
            .join("wt");
        assert_eq!(got, want);
        #[cfg(windows)]
        assert_eq!(
            git_path("C:/repo/.git/worktrees/x", Path::new("D:\\other")),
            Path::new("C:\\repo\\.git\\worktrees\\x")
        );
    }

    fn wt(path: &str, primary: &str, branch: Option<&str>) -> Worktree {
        Worktree {
            path: PathBuf::from(path),
            primary: PathBuf::from(primary),
            branch: branch.map(str::to_owned),
        }
    }

    #[test]
    fn finish_prompt_stays_one_line_whatever_the_names_hold() {
        let p = finish_prompt(
            Some("evil\nbranch\u{1b}[201~\r/quit"),
            "ma\tin",
            Path::new("/wt/x"),
            &MergeTarget::In(PathBuf::from("/p/my repo\n/x")),
        );
        assert!(
            !p.chars().any(|c| c.is_control()),
            "control characters in {p:?}"
        );
        assert!(p.contains("evil branch"));
        assert_eq!(p, sanitize_line(&p), "already sanitized");
        assert_eq!(sanitize_line("  a \n\n b\u{85}c\u{1b}d "), "a b cd");
    }

    fn tab(uid: u64, stored: &Path, live: Option<&Path>) -> TabCwd {
        TabCwd {
            uid,
            label: format!("tab{uid}"),
            stored: stored.to_path_buf(),
            live: live.map(Path::to_path_buf),
        }
    }

    #[test]
    fn classifier_matches_whole_components_and_trusts_the_live_folder() {
        let tmp = tempfile::tempdir().expect("tmp");
        let root = tmp.path();
        let (a, ab, repo) = (root.join("wt/a"), root.join("wt/ab"), root.join("repo"));
        for d in [&a, &ab, &repo, &a.join("sub")] {
            std::fs::create_dir_all(d).expect("mkdir");
        }
        let w = wt(
            a.to_str().expect("utf8"),
            repo.to_str().expect("utf8"),
            None,
        );
        let tabs = [
            tab(1, &a.join("sub"), Some(&a.join("sub"))),
            tab(2, &ab, Some(&ab)),
            // Launched in the repository, moved into the worktree by `/wt`.
            tab(3, &repo, Some(&a)),
            // Launched in the worktree, moved out since.
            tab(4, &a, Some(&repo)),
            // The worktree folder is already gone: the tail still matches.
            tab(5, &a.join("gone/deeper"), Some(&a.join("gone/deeper"))),
        ];
        assert_eq!(classify(&w, &tabs), Ok(vec![1, 3, 5]));
    }

    #[test]
    fn classifier_refuses_an_unreadable_tab_that_could_be_in_the_worktree() {
        let tmp = tempfile::tempdir().expect("tmp");
        let root = tmp.path();
        let (a, repo, other) = (root.join("wt/a"), root.join("repo"), root.join("other"));
        for d in [&a, &repo, &other, &repo.join("pkg")] {
            std::fs::create_dir_all(d).expect("mkdir");
        }
        let w = wt(
            a.to_str().expect("utf8"),
            repo.to_str().expect("utf8"),
            None,
        );
        let err = classify(&w, &[tab(7, &repo.join("pkg"), None)]).expect_err("must refuse");
        assert!(err.contains("tab7"), "{err}");
        // Launched inside the worktree: stopped; elsewhere: left alone.
        assert_eq!(
            classify(&w, &[tab(1, &a, None), tab(2, &other, None)]),
            Ok(vec![1])
        );
    }

    #[cfg(unix)]
    #[test]
    fn classifier_sees_through_symlinks() {
        let tmp = tempfile::tempdir().expect("tmp");
        let root = tmp.path();
        let (a, repo) = (root.join("wt/a"), root.join("repo"));
        std::fs::create_dir_all(&a).expect("mkdir");
        std::fs::create_dir_all(&repo).expect("mkdir");
        let link = root.join("link");
        std::os::unix::fs::symlink(&a, &link).expect("symlink");
        let into_primary = repo.join("alias");
        std::os::unix::fs::symlink(&a, &into_primary).expect("symlink");
        let w = wt(
            a.to_str().expect("utf8"),
            repo.to_str().expect("utf8"),
            None,
        );
        // A symlinked path into the worktree is inside, even from below the primary.
        assert_eq!(
            classify(
                &w,
                &[
                    tab(1, &link, Some(&link)),
                    tab(2, &repo, Some(&into_primary))
                ]
            ),
            Ok(vec![1, 2])
        );
        assert_eq!(classify(&w, &[tab(3, &repo, Some(&repo))]), Ok(vec![]));
    }

    #[cfg(windows)]
    #[test]
    fn classifier_ignores_case_on_windows() {
        let tmp = tempfile::tempdir().expect("tmp");
        let a = tmp.path().join("Wt").join("A");
        std::fs::create_dir_all(&a).expect("mkdir");
        let shouted = PathBuf::from(a.to_string_lossy().to_uppercase());
        let w = wt(
            a.to_str().expect("utf8"),
            tmp.path().to_str().expect("utf8"),
            None,
        );
        assert_eq!(classify(&w, &[tab(1, &a, Some(&shouted))]), Ok(vec![1]));
    }

    #[test]
    fn hostile_names_are_quoted_and_cannot_end_the_code_span() {
        let p = finish_prompt(
            Some("a;$(rm -rf ~)`id`'b"),
            "m`ain",
            Path::new("/wt/it's"),
            &MergeTarget::In(PathBuf::from("/p/it's")),
        );
        assert!(
            p.contains(r"merge 'a;$(rm -rf ~)`id`'\''b'"),
            "unquoted branch in {p:?}"
        );
        assert!(p.contains(r"git -C '/p/it'\''s'"));
        // Spans containing backticks use a longer fence.
        assert!(p.contains("`` git -C '/p/it'\\''s' merge"));
        assert!(p.contains("`` m`ain ``"));
        assert_eq!(code_span("plain"), "`plain`");
        let n = finish_prompt(
            Some("b"),
            "m`ain",
            Path::new("/wt/it's"),
            &MergeTarget::Nowhere,
        );
        assert!(
            n.contains(r"git -C '/wt/it'\''s' push . 'HEAD:refs/heads/m`ain'"),
            "{n:?}"
        );
    }

    #[test]
    fn finish_prompt_names_branch_base_and_primary() {
        let to = MergeTarget::In(PathBuf::from("/p/repo"));
        let p = finish_prompt(Some("feat/x"), "main", Path::new("/wt/x"), &to);
        assert!(p.contains("`feat/x`") && p.contains("`main`") && p.contains("/p/repo"));
        assert!(p.contains("`git -C '/p/repo' merge 'feat/x'`"));
        let d = finish_prompt(None, "main", Path::new("/wt/x"), &to);
        assert!(d.contains("detached HEAD"));
    }

    #[test]
    fn the_merge_goes_where_the_base_is_checked_out() {
        let list = "worktree /p/repo\nHEAD aaa\nbranch refs/heads/develop\n\nworktree /wt/main-wt\nHEAD bbb\nbranch refs/heads/main\n\nworktree /wt/x\nHEAD ccc\nbranch refs/heads/feat/x\n\nworktree /wt/d\nHEAD ddd\ndetached\n";
        assert_eq!(
            merge_target(list, "develop"),
            MergeTarget::In("/p/repo".into())
        );
        assert_eq!(
            merge_target(list, "main"),
            MergeTarget::In("/wt/main-wt".into())
        );
        assert_eq!(merge_target(list, "mai"), MergeTarget::Nowhere);
        assert_eq!(merge_target(list, "release"), MergeTarget::Nowhere);
        let n = finish_prompt(
            Some("feat/x"),
            "release",
            Path::new("/wt/x"),
            &MergeTarget::Nowhere,
        );
        assert!(
            n.contains("`git -C '/wt/x' merge 'release'`")
                && n.contains("`git -C '/wt/x' push . 'HEAD:refs/heads/release'`"),
            "{n}"
        );
        assert!(!n.contains("checked out at"));
    }

    fn status(dirty: bool, merged: bool, ahead: u32) -> WorktreeStatus {
        WorktreeStatus {
            branch: Some("b".into()),
            dirty,
            merged,
            ahead,
        }
    }

    #[test]
    fn blockers_list_every_reason_and_clean_merged_has_none() {
        assert!(blockers(&status(false, true, 0), "main").is_empty());
        assert_eq!(
            blockers(&status(true, false, 2), "main"),
            [
                "it has uncommitted or untracked changes",
                "2 commits are not merged into main"
            ]
        );
        assert_eq!(
            blockers(&status(false, false, 1), "main"),
            ["1 commit is not merged into main"]
        );
        let detached = WorktreeStatus {
            branch: None,
            ..status(false, false, 3)
        };
        assert!(blockers(&detached, "main")[0].contains("detached"));
    }
}
