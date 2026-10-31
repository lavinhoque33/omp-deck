//! Git worktree sessions, the parts without a window: how the sidebar folds the sessions of a
//! worktree into the row list of its repository, what the sidebar shows of the gateway's worktree
//! state, and the removal toast. The flows themselves run in the gateway.

use std::cmp::Reverse;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use deck_core::config::{ParkedBranch, RemovedWorktree};
use deck_core::sessions::{ProjectGroup, abbreviate_home};
use deck_core::worktree::Worktree;

use crate::folder_scope;

// ---------------------------------------------------------------------------------------------
// Sidebar layout

/// The header of a [`Family`]: a scanned group, or a session-less stand-in for a repository whose
/// own folder has no sessions (only its worktrees do).
pub enum Head {
    Group(usize),
    Synthetic(ProjectGroup),
}

/// One session row: `groups[group].sessions[session]`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Row {
    pub group: usize,
    pub session: usize,
}

/// One top-level sidebar entry: a project and its rows, newest first. The rows are the project's
/// own sessions plus the sessions of its worktrees (live or removed), which carry a branch chip.
pub struct Family {
    pub head: Head,
    pub rows: Vec<Row>,
}

impl Family {
    pub fn group<'a>(&'a self, groups: &'a [ProjectGroup]) -> &'a ProjectGroup {
        match &self.head {
            Head::Group(i) => &groups[*i],
            Head::Synthetic(g) => g,
        }
    }
}

fn modified(groups: &[ProjectGroup], r: &Row) -> SystemTime {
    groups[r.group].sessions[r.session].modified
}

/// Folds the sessions of every worktree group (and of every removed worktree) into the family of
/// its primary checkout (a synthetic header when the primary has no sessions). Rows are ordered
/// newest first, families by their newest row.
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
    let own_rows =
        |gi: usize| (0..groups[gi].sessions.len()).map(move |session| Row { group: gi, session });
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
                rows: own_rows(gi).collect(),
            }),
            None if !inside(&g.cwd) => {}
            None => {
                by_cwd.insert(g.cwd.as_path(), families.len());
                families.push(Family {
                    head: Head::Group(gi),
                    rows: own_rows(gi).collect(),
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
                rows: Vec::new(),
            });
            families.len() - 1
        });
        families[fi].rows.extend(own_rows(gi));
    }
    for f in &mut families {
        f.rows.sort_by_key(|r| Reverse(modified(groups, r)));
    }
    families.sort_by_key(|f| Reverse(f.rows.first().map(|r| modified(groups, r))));
    families
}

/// Where each parked branch is checked out, as last seen by the periodic check.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParkState {
    pub primary: PathBuf,
    pub branch: String,
    pub checked_out: Option<PathBuf>,
}

/// What the sidebar knows about parked worktrees: the records, where their branches are checked
/// out, and which worktrees were last seen clean and pushed (their remove action parks).
#[derive(Clone, Copy)]
pub struct ParkView<'a> {
    pub records: &'a [ParkedBranch],
    pub states: &'a [ParkState],
    pub parkable: &'a HashSet<PathBuf>,
}

impl<'a> ParkView<'a> {
    /// The record that lists session `id`.
    pub fn record_of(&self, id: &str) -> Option<&'a ParkedBranch> {
        self.records
            .iter()
            .find(|r| r.sessions.iter().any(|s| s == id))
    }

    /// The folder `r`'s branch is checked out in, when known.
    pub fn checked_out(&self, r: &ParkedBranch) -> Option<&'a Path> {
        self.states
            .iter()
            .find(|s| s.primary == r.primary && s.branch == r.branch)
            .and_then(|s| s.checked_out.as_deref())
    }

    pub fn is_parkable(&self, w: &Worktree) -> bool {
        self.parkable.contains(&w.path)
    }
}

/// The worktree a session row belongs to: a live linked worktree, one OmpDeck removed (its
/// sessions wait to be moved into the primary), or a parked branch (sessions already moved).
#[derive(Clone, Copy, Debug)]
pub enum RowWorktree<'a> {
    Live(&'a Worktree),
    Removed(&'a Path, &'a RemovedWorktree),
    /// The record and the folder its branch is checked out in (when known).
    Parked(&'a ParkedBranch, Option<&'a Path>),
}

impl<'a> RowWorktree<'a> {
    /// The row of session `id` in `group`: a linked worktree's, else a removed worktree's, else a
    /// parked branch's.
    pub fn of(
        group: &'a ProjectGroup,
        id: &str,
        removed: &'a BTreeMap<PathBuf, RemovedWorktree>,
        parked: &ParkView<'a>,
    ) -> Option<RowWorktree<'a>> {
        match &group.worktree {
            Some(w) => Some(RowWorktree::Live(w)),
            None => removed
                .get(&group.cwd)
                .map(|r| RowWorktree::Removed(group.cwd.as_path(), r))
                .or_else(|| {
                    parked
                        .record_of(id)
                        .map(|r| RowWorktree::Parked(r, parked.checked_out(r)))
                }),
        }
    }

    /// The branch chip's text: the branch, else the worktree folder name.
    pub fn chip_label(self) -> std::borrow::Cow<'a, str> {
        use std::borrow::Cow;
        let (branch, path) = match self {
            RowWorktree::Live(w) => (w.branch.as_deref(), w.path.as_path()),
            RowWorktree::Removed(path, r) => (r.branch.as_deref(), path),
            RowWorktree::Parked(r, _) => return Cow::Borrowed(&r.branch),
        };
        branch.map_or_else(
            || Cow::Owned(folder_scope::folder_name(path)),
            Cow::Borrowed,
        )
    }

    /// The worktree folder (none for a parked branch: it has no folder).
    pub fn path(&self) -> Option<&Path> {
        match self {
            RowWorktree::Live(w) => Some(&w.path),
            RowWorktree::Removed(p, _) => Some(p),
            RowWorktree::Parked(..) => None,
        }
    }
}

/// What a worktree's chip and menus call it: its branch, else the folder name.
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

/// Whether two session file paths are the same session: omp's `/move` changes the folder of a
/// session file but not its name (`<timestamp>_<id>.jsonl`), and a tab's recorded path may still
/// be the one from before the move.
pub fn same_session(a: &Path, b: &Path) -> bool {
    matches!((a.file_name(), b.file_name()), (Some(x), Some(y)) if x == y)
}

/// A running session as the main-checkout warning sees it: from a gateway controller (tab or
/// not) or from the live backend inventory.
#[derive(Clone, Debug)]
pub struct LiveSession {
    /// Backend session name, when it has one.
    pub name: Option<String>,
    /// Session files it has open.
    pub files: Vec<PathBuf>,
    /// Its folder (live folder, else launch folder); unknown for some backends.
    pub dir: Option<PathBuf>,
    pub label: String,
}

/// Labels of the sessions running in the main checkout `primary` itself (not in one of the
/// `worktrees`), other than the one showing `exclude`. The first entry of a session wins:
/// list controllers before the backend inventory.
pub fn main_checkout_sessions(
    live: &[LiveSession],
    primary: &Path,
    worktrees: &[PathBuf],
    exclude: &Path,
    entries: &[&deck_core::sessions::SessionEntry],
) -> Vec<String> {
    let mut seen_names: HashSet<&str> = HashSet::new();
    let mut seen_files: HashSet<&std::ffi::OsStr> = HashSet::new();
    let mut out = Vec::new();
    for s in live {
        let dup = s.name.as_deref().is_some_and(|n| seen_names.contains(n))
            || s.files
                .iter()
                .filter_map(|f| f.file_name())
                .any(|f| seen_files.contains(f));
        if let Some(n) = &s.name {
            seen_names.insert(n);
        }
        seen_files.extend(s.files.iter().filter_map(|f| f.file_name()));
        if dup || s.files.iter().any(|f| same_session(f, exclude)) {
            continue;
        }
        let Some(dir) = &s.dir else { continue };
        if dir.starts_with(primary) && !worktrees.iter().any(|w| dir.starts_with(w)) {
            // The session's own title (scan entry by file name, else by backend name); the
            // given label (tab or terminal title) only when the scan does not know it.
            let entry = s
                .files
                .iter()
                .find_map(|f| entries.iter().find(|e| same_session(&e.path, f)))
                .or_else(|| {
                    let n = s.name.as_deref()?;
                    entries
                        .iter()
                        .find(|e| deck_core::mux::session_name_for(&e.id) == n)
                });
            out.push(entry.map_or_else(|| s.label.clone(), |e| e.label().to_owned()));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

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

    /// `(head cwd, row folders newest first)` per family.
    fn heads(groups: &[ProjectGroup], f: &[Family]) -> Vec<(String, Vec<String>)> {
        f.iter()
            .map(|f| {
                (
                    f.group(groups).cwd.display().to_string(),
                    f.rows
                        .iter()
                        .map(|r| {
                            groups[r.group].sessions[r.session]
                                .cwd
                                .display()
                                .to_string()
                        })
                        .collect(),
                )
            })
            .collect()
    }

    #[test]
    fn worktree_sessions_become_rows_of_their_repository_newest_first() {
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
                    vec!["/wt/b".to_owned(), "/wt/a".to_owned(), "/p/repo".to_owned()]
                ),
                ("/p/other".to_owned(), vec!["/p/other".to_owned()]),
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
        assert_eq!(
            fam[0].rows,
            [Row {
                group: 0,
                session: 0
            }]
        );
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
            [(
                "/p/repo".to_owned(),
                vec!["/wt/a".to_owned(), "/p/repo".to_owned()]
            )]
        );
    }

    #[test]
    fn a_worktree_outside_a_scope_that_holds_only_the_worktree_is_listed_alone() {
        let groups = [
            group("/p/repo", 100, None),
            group("/wt/a", 300, Some(wt("/wt/a", "/p/repo", Some("a")))),
        ];
        let fam = layout(&groups, Some(Path::new("/wt")), &BTreeMap::new());
        assert_eq!(
            heads(&groups, &fam),
            [("/wt/a".to_owned(), vec!["/wt/a".to_owned()])]
        );
        // Nothing in scope: nothing shown, not even a synthetic parent.
        assert!(layout(&groups, Some(Path::new("/elsewhere")), &BTreeMap::new()).is_empty());
    }

    #[test]
    fn sessions_of_a_removed_worktree_are_rows_of_their_primary_with_a_removed_chip() {
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
                (
                    "/p/repo".to_owned(),
                    vec!["/wt/gone".to_owned(), "/p/repo".to_owned()]
                ),
                ("/p/plain".to_owned(), vec!["/p/plain".to_owned()]),
            ]
        );
        let none = ParkView {
            records: &[],
            states: &[],
            parkable: &HashSet::new(),
        };
        let chip = RowWorktree::of(&groups[1], "x", &removed, &none).expect("removed chip");
        assert!(matches!(chip, RowWorktree::Removed(..)));
        assert_eq!(chip.chip_label(), "gone-branch");
        assert!(RowWorktree::of(&groups[0], "x", &removed, &none).is_none());
        let no_branch = BTreeMap::from([(
            PathBuf::from("/wt/gone"),
            RemovedWorktree {
                primary: PathBuf::from("/p/repo"),
                branch: None,
            },
        )]);
        assert_eq!(
            RowWorktree::of(&groups[1], "x", &no_branch, &none)
                .expect("chip")
                .chip_label(),
            "gone"
        );
    }

    #[test]
    fn a_session_listed_in_a_park_record_gets_the_parked_chip_with_its_checkout() {
        let groups = [group("/p/repo", 100, None)];
        let records = [ParkedBranch {
            primary: PathBuf::from("/p/repo"),
            branch: "feat/x".into(),
            base: "main".into(),
            sessions: vec!["s1".into()],
            parked_ms: 1,
        }];
        let states = [ParkState {
            primary: PathBuf::from("/p/repo"),
            branch: "feat/x".into(),
            checked_out: Some(PathBuf::from("/p/repo")),
        }];
        let parkable = HashSet::new();
        let view = ParkView {
            records: &records,
            states: &states,
            parkable: &parkable,
        };
        let removed = BTreeMap::new();
        let chip = RowWorktree::of(&groups[0], "s1", &removed, &view).expect("parked chip");
        assert_eq!(chip.chip_label(), "feat/x");
        assert!(matches!(
            chip,
            RowWorktree::Parked(_, Some(p)) if p == Path::new("/p/repo")
        ));
        assert!(RowWorktree::of(&groups[0], "other", &removed, &view).is_none());
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
}

#[cfg(test)]
mod checkout_tests {
    use super::*;

    #[test]
    fn main_checkout_warning_lists_tabless_sessions_once_and_skips_worktrees() {
        let live = |name: &str, files: &[&str], dir: Option<&str>| LiveSession {
            name: Some(name.to_owned()),
            files: files.iter().map(PathBuf::from).collect(),
            dir: dir.map(PathBuf::from),
            label: name.to_owned(),
        };
        let titled = deck_core::sessions::SessionEntry {
            id: "a".into(),
            path: PathBuf::from("/moved/a"),
            cwd: PathBuf::from("/p/repo"),
            title: Some("Fix the parser".into()),
            first_prompt: None,
            modified: SystemTime::UNIX_EPOCH,
        };
        let all = [
            live("tab", &["/s/a"], Some("/p/repo")),
            live("tab", &["/s/a"], Some("/p/repo")),
            live("background", &["/s/b"], Some("/p/repo/sub")),
            live("inworktree", &["/s/c"], Some("/wt/x")),
            live("mine", &["/s/mine"], Some("/p/repo")),
            live("unknown", &["/s/d"], None),
        ];
        assert_eq!(
            main_checkout_sessions(
                &all,
                Path::new("/p/repo"),
                &[PathBuf::from("/p/repo/.wt")],
                Path::new("/s/mine"),
                &[&titled]
            ),
            ["Fix the parser", "background"]
        );
    }

    #[test]
    fn a_moved_session_file_is_the_same_session_by_name() {
        assert!(same_session(
            Path::new("/s/--wt-a--/2026-10-26T10-00-00-000Z_abc.jsonl"),
            Path::new("/s/--repo--/2026-10-26T10-00-00-000Z_abc.jsonl")
        ));
        assert!(!same_session(
            Path::new("/s/a_1.jsonl"),
            Path::new("/s/a_2.jsonl")
        ));
    }
}
