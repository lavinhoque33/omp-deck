//! Git worktree sessions, the parts without a window: how the sidebar nests worktree groups
//! under their repository, the in-memory "finishing" state of a worktree, the finish prompt and
//! the removal verdict.

use std::cmp::Reverse;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use deck_core::config::RemovedWorktree;
use deck_core::sessions::{ProjectGroup, abbreviate_home};
use deck_core::worktree::{MergeTarget, Worktree};

use crate::folder_scope;
use deck_service::statusfmt::Activity;

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
