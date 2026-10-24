//! Worktree detection, status and removal against real git repositories in temp dirs; `create`
//! against the real omp when `OMPDECK_TEST_OMP=<omp path>` is set (skipped otherwise).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use deck_core::omp::Omp;
use deck_core::sessions::{SessionsRoot, scan};
use deck_core::worktree::{
    Removed, Unsafe, Worktree, WorktreeError, check_branch_name, create, default_location,
    infer_base, remove, repo_info, resolve, status,
};

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c"])
        .arg("commit.gpgsign=false")
        .args(args)
        .env(
            "GIT_CONFIG_GLOBAL",
            if cfg!(windows) { "NUL" } else { "/dev/null" },
        )
        .env(
            "GIT_CONFIG_SYSTEM",
            if cfg!(windows) { "NUL" } else { "/dev/null" },
        )
        .env("LC_ALL", "C")
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

fn commit(dir: &Path, file: &str, body: &str) {
    fs::write(dir.join(file), body).unwrap();
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-q", "-m", file]);
}

struct Fixture {
    _tmp: tempfile::TempDir,
    /// Canonical temp root (git prints canonical paths).
    root: PathBuf,
    repo: PathBuf,
}

fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(tmp.path()).unwrap();
    // Windows canonical paths are verbatim (`\\?\C:\…`), which git can't create worktrees under;
    // git itself reports the plain drive form.
    let root = match root.to_str().and_then(|s| s.strip_prefix(r"\\?\")) {
        Some(plain) if !plain.starts_with("UNC\\") => PathBuf::from(plain),
        _ => root,
    };
    let repo = root.join("repo");
    fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    fs::write(repo.join(".gitignore"), "*.log\ntarget/\n").unwrap();
    commit(&repo, "a.txt", "a");
    Fixture {
        _tmp: tmp,
        root,
        repo,
    }
}

/// Linked worktree `name` on a new branch `br` next to the repo.
fn linked(f: &Fixture, name: &str, br: &str) -> Worktree {
    let path = f.root.join(name);
    git(
        &f.repo,
        &["worktree", "add", "-q", "-b", br, path.to_str().unwrap()],
    );
    resolve(&path).expect("linked worktree resolves")
}

#[test]
fn resolve_distinguishes_primary_linked_subdir_detached_and_non_repo() {
    let f = fixture();
    assert_eq!(resolve(&f.repo), None, "primary checkout");
    assert_eq!(resolve(&f.repo.join(".git")), None);

    let wt = linked(&f, "feat", "feature/x");
    assert_eq!(wt.path, f.root.join("feat"));
    assert_eq!(wt.primary, f.repo);
    assert_eq!(wt.branch.as_deref(), Some("feature/x"));

    let sub = wt.path.join("deep/er");
    fs::create_dir_all(&sub).unwrap();
    assert_eq!(
        resolve(&sub),
        Some(wt.clone()),
        "subdirectory of a worktree"
    );
    assert_eq!(
        resolve(&wt.path.join("not/yet/created")),
        Some(wt.clone()),
        "walks up over missing directories"
    );

    git(&wt.path, &["checkout", "-q", "--detach"]);
    let detached = resolve(&wt.path).unwrap();
    assert_eq!(detached.branch, None);
    assert_eq!(detached.primary, f.repo);

    let plain = f.root.join("plain");
    fs::create_dir(&plain).unwrap();
    assert_eq!(resolve(&plain), None, "not a repository");
}

#[test]
fn resolve_accepts_relative_gitdir_and_rejects_other_layouts() {
    let f = fixture();
    let wt = linked(&f, "rel", "rel-branch");
    // `git worktree add --relative-paths` style: the `.git` file holds a relative path.
    let rel = format!(
        "gitdir: ../repo/.git/worktrees/{}\n",
        wt.path.file_name().unwrap().to_string_lossy()
    );
    fs::write(wt.path.join(".git"), rel).unwrap();
    assert_eq!(resolve(&wt.path), Some(wt.clone()));

    // A `.git` file that is not a linked worktree (submodule-like: no commondir) → None.
    let sub = f.root.join("sub");
    fs::create_dir_all(sub.join("meta")).unwrap();
    fs::write(sub.join(".git"), "gitdir: meta\n").unwrap();
    assert_eq!(resolve(&sub), None);
    fs::write(sub.join(".git"), "garbage").unwrap();
    assert_eq!(resolve(&sub), None);
}

#[test]
fn default_location_adds_collision_suffixes() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path();
    let primary = Path::new("/src/My Repo");
    let first = default_location(base, primary, "wt/2026-10-23");
    assert_eq!(first, base.join("wt-2026-10-23-My-Repo"));
    fs::create_dir(&first).unwrap();
    let second = default_location(base, primary, "wt/2026-10-23");
    assert_eq!(second, base.join("wt-2026-10-23-My-Repo-2"));
    fs::create_dir(&second).unwrap();
    assert_eq!(
        default_location(base, primary, "wt/2026-10-23"),
        base.join("wt-2026-10-23-My-Repo-3")
    );
    // A missing base is fine: nothing exists yet.
    assert_eq!(
        default_location(&base.join("nope"), primary, "x"),
        base.join("nope").join("x-My-Repo")
    );
}

#[test]
fn status_counts_untracked_but_not_ignored_and_tracks_merge_state() {
    let f = fixture();
    let wt = linked(&f, "st", "topic");

    let s = status(&wt, "main").unwrap();
    assert_eq!(s.branch.as_deref(), Some("topic"));
    assert!(
        !s.dirty && s.merged && s.ahead == 0,
        "fresh branch is merged: {s:?}"
    );

    // Ignored files (a file and a directory) must not count as changes.
    fs::write(wt.path.join("x.log"), "noise").unwrap();
    fs::create_dir(wt.path.join("target")).unwrap();
    fs::write(wt.path.join("target/o"), "bin").unwrap();
    assert!(!status(&wt, "main").unwrap().dirty, "ignored-only is clean");

    fs::write(wt.path.join("new.txt"), "n").unwrap();
    assert!(status(&wt, "main").unwrap().dirty, "untracked counts");
    fs::remove_file(wt.path.join("new.txt")).unwrap();

    fs::write(wt.path.join("a.txt"), "changed").unwrap();
    assert!(status(&wt, "main").unwrap().dirty, "unstaged counts");
    git(&wt.path, &["add", "a.txt"]);
    assert!(status(&wt, "main").unwrap().dirty, "staged counts");
    git(&wt.path, &["commit", "-q", "-m", "c1"]);
    commit(&wt.path, "b.txt", "b");

    let s = status(&wt, "main").unwrap();
    assert!(!s.dirty && !s.merged);
    assert_eq!(s.ahead, 2);

    git(&f.repo, &["merge", "-q", "topic"]);
    let s = status(&wt, "main").unwrap();
    assert!(s.merged && s.ahead == 0, "{s:?}");

    assert!(matches!(
        status(&wt, "no-such-branch"),
        Err(WorktreeError::Git { .. })
    ));
    assert!(matches!(
        status(&wt, "--all"),
        Err(WorktreeError::Git { .. })
    ));
}

#[test]
fn remove_refuses_unsafe_states_without_touching_anything() {
    let f = fixture();
    let wt = linked(&f, "r1", "dirty-one");
    fs::write(wt.path.join("wip.txt"), "wip").unwrap();
    match remove(&wt, "main", true) {
        Err(WorktreeError::Unsafe(r)) => assert_eq!(r, vec![Unsafe::Dirty]),
        other => panic!("expected Unsafe(Dirty), got {other:?}"),
    }
    assert!(wt.path.join("wip.txt").exists());
    assert!(git(&f.repo, &["branch", "--list", "dirty-one"]).contains("dirty-one"));

    fs::remove_file(wt.path.join("wip.txt")).unwrap();
    commit(&wt.path, "work.txt", "w");
    fs::write(wt.path.join("wip2.txt"), "x").unwrap();
    match remove(&wt, "main", true) {
        Err(WorktreeError::Unsafe(r)) => {
            assert_eq!(r, vec![Unsafe::Dirty, Unsafe::NotMerged { ahead: 1 }]);
        }
        other => panic!("expected Dirty + NotMerged, got {other:?}"),
    }
    fs::remove_file(wt.path.join("wip2.txt")).unwrap();
    assert!(matches!(
        remove(&wt, "main", true),
        Err(WorktreeError::Unsafe(r)) if r == vec![Unsafe::NotMerged { ahead: 1 }]
    ));
    assert!(wt.path.join("work.txt").exists());
    assert!(
        git(&f.repo, &["branch", "--list", "dirty-one"]).contains("dirty-one"),
        "unmerged branch survives"
    );

    // Detached with commits outside the base is refused too.
    git(&wt.path, &["checkout", "-q", "--detach"]);
    let det = resolve(&wt.path).unwrap();
    assert!(matches!(
        remove(&det, "main", true),
        Err(WorktreeError::Unsafe(r)) if r == vec![Unsafe::Detached]
    ));
    assert!(wt.path.exists());
}

#[test]
fn remove_deletes_clean_merged_worktree_and_its_branch_only() {
    let f = fixture();
    let wt = linked(&f, "done", "finished");
    let other = linked(&f, "keep", "keeper");
    commit(&wt.path, "f.txt", "f");
    git(&f.repo, &["merge", "-q", "finished"]);
    // Ignored leftovers do not block removal... git refuses them without --force, so use none.
    let done = remove(&wt, "main", true).unwrap();
    assert_eq!(
        done,
        Removed {
            branch_deleted: true,
            branch_note: None
        }
    );
    assert!(!wt.path.exists());
    assert!(git(&f.repo, &["branch", "--list", "finished"]).is_empty());
    assert!(other.path.exists(), "sibling worktree untouched");
    assert!(git(&f.repo, &["branch", "--list", "keeper"]).contains("keeper"));

    // Missing directory → Unsafe::Missing, nothing run.
    assert!(matches!(
        remove(&wt, "main", true),
        Err(WorktreeError::Unsafe(r)) if r == vec![Unsafe::Missing]
    ));

    // Without delete_branch the branch stays.
    let w2 = linked(&f, "done2", "finished2");
    assert!(!remove(&w2, "main", false).unwrap().branch_deleted);
    assert!(git(&f.repo, &["branch", "--list", "finished2"]).contains("finished2"));
}

#[test]
fn branch_names_starting_with_a_dash_are_never_taken_for_options() {
    let f = fixture();
    let path = f.root.join("dash");
    git(
        &f.repo,
        &["worktree", "add", "-q", "--detach", path.to_str().unwrap()],
    );
    git(&f.repo, &["update-ref", "refs/heads/-dash", "main"]);
    git(&path, &["symbolic-ref", "HEAD", "refs/heads/-dash"]);
    let wt = resolve(&path).unwrap();
    assert_eq!(wt.branch.as_deref(), Some("-dash"));
    assert_eq!(
        status(&wt, "main").unwrap().branch.as_deref(),
        Some("-dash")
    );
    remove(&wt, "main", true).unwrap();
    assert!(!path.exists());
    assert!(git(&f.repo, &["for-each-ref", "refs/heads/-dash"]).is_empty());
}

#[test]
fn create_rejects_option_like_base_and_relative_dest_before_running_omp() {
    let f = fixture();
    let omp = Omp::at("/nonexistent/omp");
    let dest = f.root.join("never");
    assert!(matches!(
        create(&omp, &f.repo, "b", "--detach", &dest),
        Err(WorktreeError::Git { .. })
    ));
    assert!(matches!(
        create(&omp, &f.repo, "b", "main", Path::new("rel/dest")),
        Err(WorktreeError::Io(_))
    ));
}

#[test]
fn remove_deletes_the_merged_branch_when_base_is_not_checked_out_in_the_primary() {
    let f = fixture();
    // The primary stays on `main`, but the work is merged into `release`, which nobody has
    // checked out: `git branch -d` would call the branch unmerged, our base check does not.
    git(&f.repo, &["branch", "release"]);
    let wt = linked(&f, "rel", "feat-rel");
    commit(&wt.path, "r.txt", "r");
    let rel = f.root.join("rel-wt");
    git(
        &f.repo,
        &["worktree", "add", "-q", rel.to_str().unwrap(), "release"],
    );
    git(&rel, &["merge", "-q", "feat-rel"]);
    git(&f.repo, &["worktree", "remove", rel.to_str().unwrap()]);
    assert_eq!(git(&f.repo, &["rev-parse", "--abbrev-ref", "HEAD"]), "main");

    let done = remove(&wt, "release", true).unwrap();
    assert!(
        done.branch_deleted && done.branch_note.is_none(),
        "{done:?}"
    );
    assert!(!wt.path.exists());
    assert!(git(&f.repo, &["branch", "--list", "feat-rel"]).is_empty());
    assert!(git(&f.repo, &["branch", "--list", "release"]).contains("release"));
}

#[test]
fn infer_base_prefers_primary_branch_then_origin_head() {
    let f = fixture();
    let wt = linked(&f, "ib", "ib-branch");
    assert_eq!(infer_base(&wt).as_deref(), Some("main"));

    git(&f.repo, &["checkout", "-q", "-b", "dev"]);
    assert_eq!(infer_base(&wt).as_deref(), Some("dev"));

    // Primary detached, no origin → nothing to infer; with origin/HEAD → that target.
    git(&f.repo, &["checkout", "-q", "--detach"]);
    assert_eq!(infer_base(&wt), None);
    git(&f.repo, &["update-ref", "refs/remotes/origin/main", "main"]);
    git(
        &f.repo,
        &[
            "symbolic-ref",
            "refs/remotes/origin/HEAD",
            "refs/remotes/origin/main",
        ],
    );
    assert_eq!(infer_base(&wt).as_deref(), Some("origin/main"));
}

#[test]
fn repo_info_lists_branches_and_reports_non_repos() {
    let f = fixture();
    let wt = linked(&f, "ri", "zeta");
    git(&f.repo, &["branch", "alpha"]);
    git(&f.repo, &["update-ref", "refs/remotes/origin/main", "main"]);
    git(
        &f.repo,
        &[
            "symbolic-ref",
            "refs/remotes/origin/HEAD",
            "refs/remotes/origin/main",
        ],
    );

    let info = repo_info(&f.repo.join("")).unwrap();
    assert_eq!(info.root, f.repo);
    assert_eq!(info.primary, f.repo);
    assert_eq!(info.current_branch.as_deref(), Some("main"));
    assert!(info.has_commits);
    assert_eq!(info.branches, ["alpha", "main", "zeta", "origin/main"]);

    let w = repo_info(&wt.path).unwrap();
    assert_eq!(w.root, wt.path);
    assert_eq!(w.primary, f.repo, "linked worktree reports its primary");
    assert_eq!(w.current_branch.as_deref(), Some("zeta"));

    let empty = f.root.join("empty");
    fs::create_dir(&empty).unwrap();
    git(&empty, &["init", "-q", "-b", "trunk"]);
    let e = repo_info(&empty).unwrap();
    assert!(!e.has_commits && e.branches.is_empty());
    assert_eq!(e.current_branch.as_deref(), Some("trunk"));

    let plain = f.root.join("plain");
    fs::create_dir(&plain).unwrap();
    assert!(matches!(repo_info(&plain), Err(WorktreeError::NotARepo)));
    assert!(matches!(
        repo_info(&f.root.join("missing")),
        Err(WorktreeError::NotARepo)
    ));
}

#[test]
fn check_branch_name_rejects_invalid_and_existing() {
    let f = fixture();
    check_branch_name(&f.repo, "wt/20261023-1010").unwrap();
    for bad in ["", "a b", "a..b", "-x", "x.lock", "@{-1}", "a~1", "/x"] {
        assert!(
            matches!(
                check_branch_name(&f.repo, bad),
                Err(WorktreeError::Git { .. })
            ),
            "{bad:?} must be rejected"
        );
    }
    let err = check_branch_name(&f.repo, "main").unwrap_err();
    assert!(err.to_string().contains("already exists"), "{err}");
}

#[test]
fn scan_groups_carry_the_worktree_of_their_cwd() {
    let f = fixture();
    let wt = linked(&f, "sess", "sess-branch");
    let store = tempfile::tempdir().unwrap();
    for (bucket, cwd, id) in [("wt", &wt.path, "w1"), ("main", &f.repo, "m1")] {
        let dir = store.path().join(bucket);
        fs::create_dir_all(&dir).unwrap();
        let header = serde_json::json!({
            "type": "session", "version": 3, "id": id,
            "timestamp": "2026-01-01T00:00:00.000Z", "cwd": cwd,
        });
        let prompt = serde_json::json!({
            "type": "message", "id": "m",
            "message": {"role": "user", "content": "hello"},
        });
        fs::write(
            dir.join(format!("2026-01-01T00-00-00-000Z_{id}.jsonl")),
            format!("{header}\n{prompt}\n"),
        )
        .unwrap();
    }
    let groups = scan(&SessionsRoot::at(store.path()));
    assert_eq!(groups.len(), 2);
    let by = |cwd: &Path| groups.iter().find(|g| g.cwd == cwd).unwrap();
    assert_eq!(by(&wt.path).worktree.as_ref(), Some(&wt));
    assert_eq!(by(&f.repo).worktree, None);
}

fn omp_under_test() -> Option<PathBuf> {
    let p = std::env::var_os("OMPDECK_TEST_OMP").map(PathBuf::from)?;
    if p.as_os_str().is_empty() {
        return None;
    }
    Some(p)
}

/// `create` through the real omp in a sandboxed HOME / XDG / `PI_*` environment: new branch at
/// `dest` from `base_ref`, ignored files NOT copied. The sandbox keeps omp away from `~/.omp`.
#[test]
fn create_makes_a_clean_worktree_on_a_new_branch() {
    let Some(omp_bin) = omp_under_test() else {
        eprintln!("skipped: OMPDECK_TEST_OMP not set");
        return;
    };
    let f = fixture();
    let sandbox = f.root.join("home");
    fs::create_dir_all(sandbox.join("rt")).unwrap();
    // Natives are extracted into `<config root>/natives`; reuse the real ones read-only.
    if let Some(real) = dirs::home_dir().map(|h| h.join(".omp").join("natives"))
        && real.exists()
    {
        fs::create_dir_all(sandbox.join(".omp")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&real, sandbox.join(".omp").join("natives")).unwrap();
    }
    let env = |omp: Omp| {
        omp.with_env("HOME", &sandbox)
            .with_env("USERPROFILE", &sandbox)
            .with_env("XDG_CONFIG_HOME", sandbox.join("cfg"))
            .with_env("XDG_DATA_HOME", sandbox.join("data"))
            .with_env("XDG_STATE_HOME", sandbox.join("state"))
            .with_env("XDG_CACHE_HOME", sandbox.join("cache"))
            .with_env("PI_CODING_AGENT_DIR", sandbox.join("agent"))
            .with_env("PI_CONFIG_DIR", ".omp")
            .with_env("OMP_WORKTREE_DIR", sandbox.join("wt"))
    };
    let omp = env(Omp::at(omp_bin));

    git(&f.repo, &["branch", "base-x"]);
    fs::write(f.repo.join(".env"), "SECRET=1\n").unwrap();
    fs::write(f.repo.join(".gitignore"), "*.log\ntarget/\n.env\n").unwrap();
    commit(&f.repo, "base-only.txt", "later");
    assert!(git(&f.repo, &["status", "--porcelain"]).is_empty());

    let dest = sandbox.join("wt").join("made");
    create(&omp, &f.repo, "new/branch", "base-x", &dest).unwrap();

    let wt = resolve(&dest).expect("dest is a linked worktree");
    assert_eq!(wt.primary, f.repo);
    assert_eq!(wt.branch.as_deref(), Some("new/branch"));
    assert!(dest.join("a.txt").exists());
    assert!(
        !dest.join("base-only.txt").exists(),
        "created from base_ref, not from HEAD"
    );
    assert!(!dest.join(".env").exists(), "ignored files are not copied");
    assert!(
        sandbox.join("data/omp-deck/worktree-overlay.yml").exists() || !cfg!(target_os = "linux"),
        "overlay is written under the configured (sandbox) data dir"
    );

    // A second create onto the same branch fails with omp's message.
    let err = create(
        &omp,
        &f.repo,
        "new/branch",
        "base-x",
        &sandbox.join("wt/again"),
    )
    .unwrap_err();
    assert!(
        matches!(err, WorktreeError::Omp { ref stderr } if !stderr.is_empty()),
        "{err:?}"
    );
}
