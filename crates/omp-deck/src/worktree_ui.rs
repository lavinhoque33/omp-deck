//! Git worktree sessions, the parts without a window: how the sidebar nests worktree groups
//! under their repository, the in-memory "finishing" state of a worktree, the finish prompt and
//! the removal verdict.

use std::cmp::Reverse;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use deck_core::config::RemovedWorktree;
use deck_core::sessions::{ProjectGroup, abbreviate_home};
use deck_core::worktree::{Worktree, WorktreeStatus};

use crate::folder_scope;
use crate::statusfmt::Activity;

/// Label of the injected finish prompt (the injector reports refusals by label).
pub const FINISH_LABEL: &str = "Finish worktree";
/// Label of the injected `/wt` command.
pub const MOVE_LABEL: &str = "Move to worktree";

/// How long a finish waits for its tab (a closed session is resumed first) to reach an idle
/// composer before it gives up.
pub const FINISH_TAB_TIMEOUT: Duration = Duration::from_secs(90);

// ---------------------------------------------------------------------------------------------
// Sidebar nesting

/// The header of a [`Family`]: a scanned group, or a session-less stand-in for a repository whose
/// own folder has no sessions (only its worktrees do).
pub enum Head {
    Group(usize),
    Synthetic(ProjectGroup),
}

/// One top-level sidebar entry: a project group with the worktree groups nested under it.
/// A plain project is a family without children.
pub struct Family {
    pub head: Head,
    /// Indices of the worktree groups, newest first.
    pub children: Vec<usize>,
}

impl Family {
    pub fn group<'a>(&'a self, groups: &'a [ProjectGroup]) -> &'a ProjectGroup {
        match &self.head {
            Head::Group(i) => &groups[*i],
            Head::Synthetic(g) => g,
        }
    }
}

fn newest(g: &ProjectGroup) -> Option<SystemTime> {
    g.sessions.iter().map(|s| s.modified).max()
}

/// Nests every worktree group under the group of its primary checkout (a synthetic header when
/// the primary has no sessions). Families and their children are ordered newest first; a
/// family's age is the newest of the header and its children.
///
/// Per-folder scope: a repository inside the scope brings all its worktrees along, wherever they
/// live (omp keeps them under its own managed folder). A worktree whose repository is outside
/// the scope is listed by itself when its own folder is inside.
pub fn layout(
    groups: &[ProjectGroup],
    scope: Option<&Path>,
    removed: &BTreeMap<PathBuf, RemovedWorktree>,
) -> Vec<Family> {
    let inside = |p: &Path| scope.is_none_or(|s| folder_scope::contains(s, p));
    let mut families: Vec<Family> = Vec::new();
    let mut by_cwd: HashMap<&Path, usize> = HashMap::new();
    let mut children: Vec<(usize, &Path)> = Vec::new();
    for (gi, g) in groups.iter().enumerate() {
        let parent = g
            .worktree
            .as_ref()
            .map(|w| w.primary.as_path())
            .or_else(|| removed.get(&g.cwd).map(|r| r.primary.as_path()));
        match parent {
            Some(p) if inside(p) => children.push((gi, p)),
            Some(_) if !inside(&g.cwd) => {}
            Some(_) => families.push(Family {
                head: Head::Group(gi),
                children: Vec::new(),
            }),
            None if !inside(&g.cwd) => {}
            None => {
                by_cwd.insert(g.cwd.as_path(), families.len());
                families.push(Family {
                    head: Head::Group(gi),
                    children: Vec::new(),
                });
            }
        }
    }
    for (gi, primary) in children {
        let fi = *by_cwd.entry(primary).or_insert_with(|| {
            families.push(Family {
                head: Head::Synthetic(ProjectGroup {
                    cwd: primary.to_path_buf(),
                    display: abbreviate_home(primary),
                    sessions: Vec::new(),
                    worktree: None,
                }),
                children: Vec::new(),
            });
            families.len() - 1
        });
        families[fi].children.push(gi);
    }
    for f in &mut families {
        f.children.sort_by_key(|&c| Reverse(newest(&groups[c])));
    }
    let age = |f: &Family| {
        f.children
            .iter()
            .map(|&c| newest(&groups[c]))
            .chain(std::iter::once(newest(f.group(groups))))
            .max()
            .flatten()
    };
    families.sort_by_key(|f| Reverse(age(f)));
    families
}

/// Entries of the removed-worktree map whose primary checkout no longer exists (blocking:
/// filesystem; call from a worker).
pub fn orphaned_removed(removed: &BTreeMap<PathBuf, RemovedWorktree>) -> Vec<PathBuf> {
    removed
        .iter()
        .filter(|(_, r)| !r.primary.is_dir())
        .map(|(p, _)| p.clone())
        .collect()
}

/// What a removed worktree's group header shows.
pub fn removed_label(path: &Path, r: &RemovedWorktree) -> String {
    let name = r
        .branch
        .clone()
        .unwrap_or_else(|| folder_scope::folder_name(path));
    format!("{name} (removed)")
}

/// What a worktree group's header shows: its branch, else the folder name.
pub fn worktree_label(w: &Worktree) -> String {
    w.branch
        .clone()
        .unwrap_or_else(|| folder_scope::folder_name(&w.path))
}

/// Whether `dir` is the worktree folder or lies inside it, compared lexically. Cheap enough for
/// the UI thread (picking the tab that receives the finish prompt); removal uses [`classify`].
pub fn inside(w: &Worktree, dir: &Path) -> bool {
    dir.starts_with(&w.path)
}

// ---------------------------------------------------------------------------------------------
// Which tabs run inside a worktree (blocking: reads the file system; call from a worker)

/// A running session tab as the classifier sees it.
#[derive(Clone, Debug)]
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
    let mut tail: Vec<&std::ffi::OsStr> = Vec::new();
    let mut cur = p;
    loop {
        if let Ok(c) = std::fs::canonicalize(cur) {
            let mut out = strip_verbatim(c);
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

#[cfg(windows)]
fn strip_verbatim(p: PathBuf) -> PathBuf {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        PathBuf::from(format!(r"\\{rest}"))
    } else if let Some(rest) = s.strip_prefix(r"\\?\") {
        PathBuf::from(rest.to_owned())
    } else {
        p
    }
}

#[cfg(not(windows))]
fn strip_verbatim(p: PathBuf) -> PathBuf {
    p
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

/// Whether canonical `dir` is `root` or below it, by whole components (`/wt/a` does not contain
/// `/wt/ab`).
fn contains_keys(root: &[String], dir: &[String]) -> bool {
    dir.starts_with(root)
}

/// The tabs to stop when the worktree goes away: those whose live folder is inside it. A tab
/// whose live folder is unknown counts when it was launched inside the worktree; one launched in
/// the worktree's repository (where `/wt` could have moved it into the worktree) cannot be
/// judged, so the error names it and removal must not proceed. Everything else is unrelated.
pub fn classify(wt: &Worktree, tabs: &[TabCwd]) -> Result<Vec<u64>, String> {
    let root = keys(&canonical(&wt.path));
    let primary = keys(&canonical(&wt.primary));
    let mut stop = Vec::new();
    for t in tabs {
        let hit = match &t.live {
            Some(live) => contains_keys(&root, &keys(&canonical(live))),
            None => {
                let stored = keys(&canonical(&t.stored));
                if contains_keys(&root, &stored) {
                    true
                } else if contains_keys(&primary, &stored) || contains_keys(&stored, &primary) {
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

// ---------------------------------------------------------------------------------------------
// Branch names

/// `wt/yyyymmdd-hhmm` for a Unix time (UTC; the name is only a suggestion).
pub fn branch_stamp(unix_secs: u64) -> String {
    let days = (unix_secs / 86_400) as i64;
    let rem = unix_secs % 86_400;
    // Days since 1970-01-01 → civil date (Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "wt/{year:04}{month:02}{day:02}-{:02}{:02}",
        rem / 3_600,
        rem % 3_600 / 60
    )
}

/// The branch to record as a new worktree's base: the local branch when the user picked its
/// remote-tracking `origin/…` twin, nothing for a detached `HEAD` (merge targets are then inferred).
pub fn base_to_record(base_ref: &str, branches: &[String]) -> Option<String> {
    if base_ref == "HEAD" {
        return None;
    }
    let local = base_ref
        .strip_prefix("origin/")
        .filter(|rest| branches.iter().any(|b| b == rest));
    Some(local.unwrap_or(base_ref).to_owned())
}

// ---------------------------------------------------------------------------------------------
// Finishing

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

/// Where the branch has to be merged so that `base` advances.
#[derive(Clone, Debug, PartialEq, Eq)]
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
    deck_core::platform::no_window(&mut cmd);
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

/// The one-line prompt sent to the worktree's session. The injector sanitizes it again, so a
/// branch or path with control characters can never break out of the paste.
pub fn finish_prompt(
    branch: Option<&str>,
    base: &str,
    worktree: &Path,
    target: &MergeTarget,
) -> String {
    use crate::sidebar::shell_quote as q;
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
    crate::inject::sanitize_line(&format!(
        "Finish this worktree: commit any remaining work on {work}, {how}. Follow this project's rules on committing and merging; if they do not let you do that without being asked, ask me first. Do not delete the worktree or its branch — OmpDeck removes both once everything is merged. When you are done, tell me the result."
    ))
}

/// The toast after a removal: the folder is gone; the branch may have been kept.
pub fn removal_text(label: &str, branch: Option<&str>, r: &deck_core::worktree::Removed) -> String {
    match (branch, r.branch_deleted, &r.branch_note) {
        (None, _, None) => format!("Removed worktree “{label}”"),
        (None, _, Some(note)) => format!("Removed worktree “{label}”; {note}"),
        (Some(_), true, _) => format!("Removed worktree “{label}” and its branch"),
        (Some(b), false, Some(note)) => {
            format!("Removed worktree “{label}”; kept branch {b}: {note}")
        }
        (Some(b), false, None) => format!("Removed worktree “{label}”; kept branch {b}"),
    }
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

/// What to do after a state observation of a finishing worktree.
#[derive(Debug, PartialEq, Eq)]
pub enum Step {
    Nothing,
    /// Type the finish prompt into the tab (it is idle now).
    Inject,
    /// The agent's turn ended: check the worktree.
    Recheck,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Clean and merged: stop its tabs and remove it.
    Remove,
    /// Keep waiting; `announce` is the one-time explanation.
    Wait { announce: Option<String> },
}

/// A worktree that was sent the finish prompt (in memory only).
#[derive(Debug)]
pub struct Finishing {
    pub worktree: Worktree,
    pub base: String,
    /// The tab (uid) that runs the finish; unknown while a closed session is being resumed.
    pub uid: Option<u64>,
    /// The session to open when no tab runs the worktree.
    pub session: Option<PathBuf>,
    pub started: Instant,
    /// Where to merge, known once a worker looked at the checkouts.
    pub target: Option<MergeTarget>,
    injected: bool,
    retry_at: Option<Instant>,
    was_busy: bool,
    checking: bool,
    warned: bool,
}

impl Finishing {
    pub fn new(
        worktree: Worktree,
        base: String,
        uid: Option<u64>,
        session: Option<PathBuf>,
        now: Instant,
    ) -> Finishing {
        Finishing {
            worktree,
            base,
            uid,
            session,
            started: now,
            target: None,
            injected: false,
            retry_at: None,
            was_busy: false,
            checking: false,
            warned: false,
        }
    }

    /// The tab's activity after this frame. Nothing is typed before the composer is idle; after
    /// the prompt, every busy → idle transition asks for one check at a time.
    pub fn observe(&mut self, activity: Activity) -> Step {
        if !self.injected {
            return if activity == Activity::Idle {
                Step::Inject
            } else {
                Step::Nothing
            };
        }
        match activity {
            Activity::Busy => {
                self.was_busy = true;
                Step::Nothing
            }
            Activity::Idle if self.was_busy && !self.checking => {
                self.was_busy = false;
                self.checking = true;
                Step::Recheck
            }
            _ => Step::Nothing,
        }
    }

    pub fn mark_injected(&mut self) {
        self.injected = true;
    }

    /// The typed prompt never arrived (omp was not ready, a dialog was open, …): offer it again
    /// after a pause.
    pub fn injection_failed(&mut self, now: Instant) {
        self.injected = false;
        self.retry_at = Some(now + Duration::from_millis(1500));
    }

    /// Whether the prompt may be typed now: the target is known and no retry pause is running.
    pub fn may_inject(&self, now: Instant) -> bool {
        self.target.is_some() && self.retry_at.is_none_or(|t| now >= t)
    }

    pub fn injected(&self) -> bool {
        self.injected
    }

    /// The check's answer: `blockers` from [`blockers`].
    pub fn on_checked(&mut self, blockers: &[String]) -> Verdict {
        self.checking = false;
        if blockers.is_empty() {
            return Verdict::Remove;
        }
        Verdict::Wait {
            announce: self.announce_once(format!(
                "{} is not finished yet: {}. OmpDeck keeps waiting and checks again after the agent's next turn.",
                worktree_label(&self.worktree),
                blockers.join("; ")
            )),
        }
    }

    /// The check failed (git error): explained once, the wait goes on.
    pub fn on_check_failed(&mut self, error: &str) -> Option<String> {
        self.checking = false;
        self.announce_once(format!(
            "Could not check {}: {error}",
            worktree_label(&self.worktree)
        ))
    }

    fn announce_once(&mut self, text: String) -> Option<String> {
        if self.warned {
            None
        } else {
            self.warned = true;
            Some(text)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wt(path: &str, primary: &str, branch: Option<&str>) -> Worktree {
        Worktree {
            path: PathBuf::from(path),
            primary: PathBuf::from(primary),
            branch: branch.map(str::to_owned),
        }
    }

    fn group(cwd: &str, age: u64, worktree: Option<Worktree>) -> ProjectGroup {
        let modified = SystemTime::UNIX_EPOCH + Duration::from_secs(age);
        ProjectGroup {
            cwd: PathBuf::from(cwd),
            display: cwd.to_owned(),
            sessions: vec![deck_core::sessions::SessionEntry {
                id: cwd.to_owned(),
                path: PathBuf::from(format!("{cwd}/s.jsonl")),
                cwd: PathBuf::from(cwd),
                title: None,
                first_prompt: None,
                modified,
            }],
            worktree,
        }
    }

    fn heads(groups: &[ProjectGroup], f: &[Family]) -> Vec<(String, Vec<String>)> {
        f.iter()
            .map(|f| {
                (
                    f.group(groups).cwd.display().to_string(),
                    f.children
                        .iter()
                        .map(|&c| groups[c].cwd.display().to_string())
                        .collect(),
                )
            })
            .collect()
    }

    #[test]
    fn worktrees_nest_under_their_repository_and_the_newest_child_lifts_the_family() {
        let groups = [
            group("/p/other", 500, None),
            group("/p/repo", 100, None),
            group("/wt/a", 300, Some(wt("/wt/a", "/p/repo", Some("a")))),
            group("/wt/b", 900, Some(wt("/wt/b", "/p/repo", Some("b")))),
        ];
        let fam = layout(&groups, None, &BTreeMap::new());
        assert_eq!(
            heads(&groups, &fam),
            [
                (
                    "/p/repo".to_owned(),
                    vec!["/wt/b".to_owned(), "/wt/a".to_owned()]
                ),
                ("/p/other".to_owned(), vec![]),
            ]
        );
    }

    #[test]
    fn a_repository_without_sessions_gets_a_synthetic_header() {
        let groups = [group("/wt/a", 300, Some(wt("/wt/a", "/p/repo", Some("a"))))];
        let fam = layout(&groups, None, &BTreeMap::new());
        assert_eq!(fam.len(), 1);
        let Head::Synthetic(head) = &fam[0].head else {
            panic!("expected a synthetic header");
        };
        assert_eq!(head.cwd, Path::new("/p/repo"));
        assert!(head.sessions.is_empty());
        assert_eq!(fam[0].children, [0]);
    }

    #[test]
    fn scope_takes_the_worktrees_of_a_repository_inside_it_wherever_they_live() {
        let groups = [
            group("/p/repo", 100, None),
            group("/wt/a", 300, Some(wt("/wt/a", "/p/repo", Some("a")))),
            group("/p/else", 400, None),
            group("/wt/c", 500, Some(wt("/wt/c", "/p/else", Some("c")))),
        ];
        let fam = layout(&groups, Some(Path::new("/p/repo")), &BTreeMap::new());
        assert_eq!(
            heads(&groups, &fam),
            [("/p/repo".to_owned(), vec!["/wt/a".to_owned()])]
        );
    }

    #[test]
    fn a_worktree_outside_a_scope_that_holds_only_the_worktree_is_listed_alone() {
        let groups = [
            group("/p/repo", 100, None),
            group("/wt/a", 300, Some(wt("/wt/a", "/p/repo", Some("a")))),
        ];
        let fam = layout(&groups, Some(Path::new("/wt")), &BTreeMap::new());
        assert_eq!(heads(&groups, &fam), [("/wt/a".to_owned(), vec![])]);
        // Nothing in scope: nothing shown, not even a synthetic parent.
        assert!(layout(&groups, Some(Path::new("/elsewhere")), &BTreeMap::new()).is_empty());
    }

    #[test]
    fn sessions_of_a_removed_worktree_nest_under_their_primary() {
        let groups = [
            group("/p/repo", 100, None),
            group("/wt/gone", 300, None),
            group("/p/plain", 200, None),
        ];
        let mut removed = BTreeMap::new();
        removed.insert(
            PathBuf::from("/wt/gone"),
            RemovedWorktree {
                primary: PathBuf::from("/p/repo"),
                branch: Some("gone-branch".into()),
            },
        );
        let fam = layout(&groups, None, &removed);
        assert_eq!(
            heads(&groups, &fam),
            [
                ("/p/repo".to_owned(), vec!["/wt/gone".to_owned()]),
                ("/p/plain".to_owned(), vec![]),
            ]
        );
        assert_eq!(
            removed_label(Path::new("/wt/gone"), &removed[Path::new("/wt/gone")]),
            "gone-branch (removed)"
        );
        let no_branch = RemovedWorktree {
            primary: PathBuf::from("/p/repo"),
            branch: None,
        };
        assert_eq!(
            removed_label(Path::new("/wt/gone"), &no_branch),
            "gone (removed)"
        );
    }

    #[test]
    fn entries_whose_primary_vanished_are_orphaned() {
        let tmp = tempfile::tempdir().expect("tmp");
        let mut removed = BTreeMap::new();
        let entry = |p: PathBuf| RemovedWorktree {
            primary: p,
            branch: None,
        };
        removed.insert(PathBuf::from("/wt/a"), entry(tmp.path().to_path_buf()));
        removed.insert(PathBuf::from("/wt/b"), entry(tmp.path().join("missing")));
        assert_eq!(orphaned_removed(&removed), [PathBuf::from("/wt/b")]);
    }

    #[test]
    fn labels_fall_back_to_the_folder_name() {
        assert_eq!(
            worktree_label(&wt("/wt/a-repo", "/p/repo", Some("feat/x"))),
            "feat/x"
        );
        assert_eq!(worktree_label(&wt("/wt/a-repo", "/p/repo", None)), "a-repo");
        assert!(inside(&wt("/wt/a", "/p", None), Path::new("/wt/a/sub")));
        assert!(!inside(&wt("/wt/a", "/p", None), Path::new("/wt/ab")));
    }

    #[test]
    fn branch_stamp_is_a_utc_date_and_minute() {
        assert_eq!(branch_stamp(0), "wt/19700101-0000");
        // 2026-10-23 10:07:59 UTC
        assert_eq!(branch_stamp(1_792_750_079), "wt/20261023-1007");
        // Leap day.
        assert_eq!(
            branch_stamp(1_709_164_800 + 3_600 * 23 + 59 * 60),
            "wt/20240229-2359"
        );
    }

    #[test]
    fn recorded_base_prefers_the_local_twin_of_a_remote_ref() {
        let branches = vec![
            "main".to_owned(),
            "origin/main".to_owned(),
            "origin/only".to_owned(),
        ];
        assert_eq!(base_to_record("main", &branches).as_deref(), Some("main"));
        assert_eq!(
            base_to_record("origin/main", &branches).as_deref(),
            Some("main")
        );
        assert_eq!(
            base_to_record("origin/only", &branches).as_deref(),
            Some("origin/only")
        );
        assert_eq!(base_to_record("HEAD", &branches), None);
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
        assert_eq!(p, crate::inject::sanitize_line(&p), "already sanitized");
        let job = crate::inject::Job::slash(FINISH_LABEL, &p, crate::inject::Requires::Idle);
        assert_eq!(job.payload, crate::inject::Payload::Slash(p));
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
    fn a_dropped_prompt_is_offered_again_after_a_pause() {
        let t0 = Instant::now();
        let mut f = finishing();
        assert!(!f.may_inject(t0), "no merge target yet");
        f.target = Some(MergeTarget::Nowhere);
        assert!(f.may_inject(t0));
        assert_eq!(f.observe(Activity::Idle), Step::Inject);
        f.mark_injected();
        f.injection_failed(t0);
        assert!(!f.injected() && !f.may_inject(t0));
        assert!(f.may_inject(t0 + Duration::from_secs(2)));
        assert_eq!(f.observe(Activity::Idle), Step::Inject);
    }

    #[test]
    fn removal_toast_matches_what_happened() {
        use deck_core::worktree::Removed;
        let r = |deleted, note: Option<&str>| Removed {
            branch_deleted: deleted,
            branch_note: note.map(str::to_owned),
        };
        assert_eq!(
            removal_text("a", Some("a"), &r(true, None)),
            "Removed worktree “a” and its branch"
        );
        assert_eq!(
            removal_text("a", Some("a"), &r(false, Some("it moved"))),
            "Removed worktree “a”; kept branch a: it moved"
        );
        assert_eq!(
            removal_text("d", None, &r(false, None)),
            "Removed worktree “d”"
        );
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

    fn finishing() -> Finishing {
        Finishing::new(
            wt("/wt/a", "/p/repo", Some("a")),
            "main".into(),
            Some(1),
            None,
            Instant::now(),
        )
    }

    #[test]
    fn finish_types_the_prompt_only_into_an_idle_composer() {
        let mut f = finishing();
        for a in [Activity::Unknown, Activity::Busy, Activity::Attention] {
            assert_eq!(f.observe(a), Step::Nothing, "{a:?}");
        }
        assert_eq!(f.observe(Activity::Idle), Step::Inject);
        // Not injected yet (the job may still be refused): keeps asking.
        assert_eq!(f.observe(Activity::Idle), Step::Inject);
        f.mark_injected();
        assert_eq!(
            f.observe(Activity::Idle),
            Step::Nothing,
            "idle before the turn started"
        );
    }

    #[test]
    fn finish_checks_once_per_busy_to_idle_transition() {
        let mut f = finishing();
        f.mark_injected();
        assert_eq!(f.observe(Activity::Busy), Step::Nothing);
        assert_eq!(
            f.observe(Activity::Attention),
            Step::Nothing,
            "asking the user is not the end"
        );
        assert_eq!(f.observe(Activity::Idle), Step::Recheck);
        assert_eq!(
            f.observe(Activity::Idle),
            Step::Nothing,
            "one check in flight"
        );
        assert_eq!(f.observe(Activity::Busy), Step::Nothing);
        assert_eq!(
            f.observe(Activity::Idle),
            Step::Nothing,
            "previous answer still pending"
        );
        let _ = f.on_checked(&["x".to_owned()]);
        assert_eq!(f.observe(Activity::Busy), Step::Nothing);
        assert_eq!(f.observe(Activity::Idle), Step::Recheck);
    }

    #[test]
    fn an_unfinished_worktree_is_explained_once_and_a_clean_merged_one_is_removed() {
        let mut f = finishing();
        f.mark_injected();
        let reasons = vec!["1 commit is not merged into main".to_owned()];
        let Verdict::Wait {
            announce: Some(text),
        } = f.on_checked(&reasons)
        else {
            panic!("first wait announces");
        };
        assert!(text.contains("a is not finished yet") && text.contains("1 commit is not merged"));
        assert_eq!(f.on_checked(&reasons), Verdict::Wait { announce: None });
        assert_eq!(f.on_check_failed("boom"), None, "already explained");
        assert_eq!(f.on_checked(&[]), Verdict::Remove);
    }
}
