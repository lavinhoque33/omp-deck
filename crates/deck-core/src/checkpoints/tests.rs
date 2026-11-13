use super::*;

struct Repo(tempfile::TempDir);

impl Repo {
    fn new() -> Self {
        let repo = Self(tempfile::tempdir().unwrap());
        repo.git(&["init", "--initial-branch=main"]);
        repo.git(&["config", "core.autocrlf", "false"]);
        repo
    }

    fn path(&self) -> &Path {
        self.0.path()
    }

    fn git(&self, args: &[&str]) -> String {
        text(checked(self.path(), args, None).unwrap()).unwrap()
    }

    fn write(&self, path: &str, contents: &[u8]) {
        let path = self.path().join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    fn commit(&self) {
        self.git(&["add", "-A"]);
        self.git(&["commit", "-m", "fixture"]);
    }

    fn capture(&self) -> Checkpoint {
        create(
            self.path(),
            "session",
            CheckpointKind::Turn,
            "Implement the feature",
        )
        .unwrap()
        .unwrap()
    }

    fn index(&self) -> Option<Vec<u8>> {
        let path = self.git(&["rev-parse", "--path-format=absolute", "--git-path", "index"]);
        match fs::read(path) {
            Ok(bytes) => Some(bytes),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => panic!("reading index: {error}"),
        }
    }
}

#[test]
fn capture_uses_working_tree_and_preserves_staged_index_and_metadata() {
    let repo = Repo::new();
    repo.write("tracked", b"original\n");
    repo.commit();
    repo.write("tracked", b"staged\n");
    repo.git(&["add", "tracked"]);
    repo.write("tracked", b"unstaged\n");
    repo.write("untracked", b"new\n");
    let index = repo.index();
    let checkpoint = create(
        repo.path(),
        "session",
        CheckpointKind::Rollback,
        "  safety\n copy\t\u{1b} ",
    )
    .unwrap()
    .unwrap();
    assert_eq!(checkpoint.seq, 1);
    assert_eq!(checkpoint.kind, CheckpointKind::Rollback);
    assert_eq!(checkpoint.prompt, "safety copy");
    assert_eq!(checkpoint.entry, None);
    assert!(checkpoint.at_unix_ms > 0);
    assert_eq!(
        repo.git(&["show", &format!("{}:tracked", checkpoint.commit)]),
        "unstaged"
    );
    assert_eq!(
        repo.git(&["show", &format!("{}:untracked", checkpoint.commit)]),
        "new"
    );
    assert_eq!(repo.index(), index);
    assert_eq!(
        fs::read(repo.path().join("tracked")).unwrap(),
        b"unstaged\n"
    );
    assert_eq!(
        list(repo.path(), "session").unwrap().as_slice(),
        std::slice::from_ref(&checkpoint)
    );
    assert!(list(repo.path(), "other").unwrap().is_empty());
    assert!(
        create(repo.path(), "session", CheckpointKind::Turn, "same tree")
            .unwrap()
            .is_none()
    );
    assert_eq!(repo.index(), index);
    let value = serde_json::to_value(&checkpoint).unwrap();
    assert_eq!(
        serde_json::from_value::<Checkpoint>(value).unwrap(),
        checkpoint
    );
}

#[test]
fn newest_fifty_are_retained_and_sequences_do_not_reuse_pruned_refs() {
    let repo = Repo::new();
    repo.write("tracked", b"first\n");
    repo.commit();
    let first = repo.capture();
    for seq in 2..=55 {
        repo.git(&[
            "update-ref",
            &format!("refs/ompdeck/checkpoints/session/{seq:06}"),
            &first.commit,
        ]);
    }
    repo.write("tracked", b"second\n");
    let newest = repo.capture();
    assert_eq!(newest.seq, 56);
    let checkpoints = list(repo.path(), "session").unwrap();
    assert_eq!(checkpoints.len(), 50);
    assert_eq!(checkpoints.first().unwrap().seq, 56);
    assert_eq!(checkpoints.last().unwrap().seq, 7);
    assert!(
        !repo
            .git(&[
                "for-each-ref",
                "--format=%(refname)",
                "refs/ompdeck/checkpoints/session/"
            ])
            .contains("/000006")
    );
}

#[test]
fn restore_handles_renames_file_directory_transitions_and_untracked_files() {
    let repo = Repo::new();
    for (path, text) in [
        ("old", "renamed content\n"),
        ("file", "was a file\n"),
        ("dir/leaf", "was a directory\n"),
        ("tracked", "original\n"),
    ] {
        repo.write(path, text.as_bytes());
    }
    repo.commit();
    repo.write("snapshot-untracked", b"restore this too\n");
    let checkpoint = repo.capture();
    fs::rename(repo.path().join("old"), repo.path().join("new")).unwrap();
    fs::remove_file(repo.path().join("file")).unwrap();
    repo.write("file/leaf", b"now a directory\n");
    fs::remove_file(repo.path().join("dir/leaf")).unwrap();
    fs::remove_dir(repo.path().join("dir")).unwrap();
    repo.write("dir", b"now a file\n");
    fs::remove_file(repo.path().join("snapshot-untracked")).unwrap();
    repo.write("added/deep/new", b"remove me\n");
    repo.write("tracked", b"staged after checkpoint\n");
    repo.git(&["add", "tracked"]);
    repo.write("tracked", b"unstaged after checkpoint\n");
    let index = repo.index();
    let preview = diff_stat(repo.path(), &checkpoint.commit).unwrap();
    assert!(preview.iter().any(|delta| delta.status == 'R'
        && delta.path == "new"
        && delta.old_path.as_deref() == Some("old")));
    assert!(
        preview
            .iter()
            .any(|delta| delta.path == "added/deep/new" && delta.added == Some(1))
    );
    let report = restore(repo.path(), &checkpoint.commit).unwrap();
    assert_eq!(report.changed as usize, preview.len());
    assert_eq!(repo.index(), index);
    assert_eq!(
        fs::read(repo.path().join("old")).unwrap(),
        b"renamed content\n"
    );
    assert!(!repo.path().join("new").exists());
    assert_eq!(fs::read(repo.path().join("file")).unwrap(), b"was a file\n");
    assert_eq!(
        fs::read(repo.path().join("dir/leaf")).unwrap(),
        b"was a directory\n"
    );
    assert_eq!(
        fs::read(repo.path().join("tracked")).unwrap(),
        b"original\n"
    );
    assert_eq!(
        fs::read(repo.path().join("snapshot-untracked")).unwrap(),
        b"restore this too\n"
    );
    assert!(!repo.path().join("added").exists());
    assert!(
        diff_stat(repo.path(), &checkpoint.commit)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn ignored_content_is_not_captured_or_removed() {
    let repo = Repo::new();
    repo.write(".gitignore", b"ignored/\n");
    repo.write("tracked", b"before\n");
    repo.write("ignored/keep", b"private before\n");
    repo.commit();
    let checkpoint = repo.capture();
    repo.write("tracked", b"after\n");
    repo.write("ignored/keep", b"private after\n");
    let index = repo.index();
    restore(repo.path(), &checkpoint.commit).unwrap();
    assert_eq!(
        fs::read(repo.path().join("ignored/keep")).unwrap(),
        b"private after\n"
    );
    assert_eq!(fs::read(repo.path().join("tracked")).unwrap(), b"before\n");
    assert_eq!(repo.index(), index);
}

#[test]
fn ignored_collisions_are_refused_before_any_mutation() {
    for (target, ignored) in [
        ("protected", "protected"),
        ("protected", "protected/secret"),
        ("protected/leaf", "protected"),
    ] {
        let repo = Repo::new();
        repo.write(target, b"checkpoint content\n");
        repo.write("tracked", b"checkpoint tracked\n");
        repo.commit();
        let checkpoint = repo.capture();
        repo.git(&["rm", "-r", "--cached", "--", "protected"]);
        fs::remove_file(repo.path().join(target)).unwrap();
        if target.contains('/') {
            fs::remove_dir(repo.path().join("protected")).unwrap();
        }
        repo.write(".gitignore", b"protected\n");
        repo.write(ignored, b"must survive\n");
        repo.write("tracked", b"current tracked\n");
        let index = repo.index();
        assert!(matches!(
            restore(repo.path(), &checkpoint.commit),
            Err(CheckpointError::IgnoredCollision { .. })
        ));
        assert_eq!(
            fs::read(repo.path().join(ignored)).unwrap(),
            b"must survive\n"
        );
        assert_eq!(
            fs::read(repo.path().join("tracked")).unwrap(),
            b"current tracked\n"
        );
        assert_eq!(repo.index(), index);
    }
}

#[cfg(unix)]
#[test]
fn restore_preserves_symlink_targets_and_does_not_follow_them() {
    use std::os::unix::fs::symlink;
    let repo = Repo::new();
    repo.write("target", b"original\n");
    symlink("target", repo.path().join("link")).unwrap();
    repo.commit();
    let checkpoint = repo.capture();
    fs::remove_file(repo.path().join("link")).unwrap();
    symlink("other", repo.path().join("link")).unwrap();
    repo.write("other", b"untracked other\n");
    let index = repo.index();
    restore(repo.path(), &checkpoint.commit).unwrap();
    assert_eq!(
        fs::read_link(repo.path().join("link")).unwrap(),
        Path::new("target")
    );
    assert_eq!(fs::read(repo.path().join("link")).unwrap(), b"original\n");
    assert!(!repo.path().join("other").exists());
    assert_eq!(repo.index(), index);
}

#[cfg(unix)]
#[test]
fn restore_reinstates_executable_mode() {
    use std::os::unix::fs::PermissionsExt;
    let repo = Repo::new();
    repo.git(&["config", "core.filemode", "true"]);
    repo.write("script", b"#!/bin/sh\nexit 0\n");
    let path = repo.path().join("script");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    repo.commit();
    let checkpoint = repo.capture();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    let index = repo.index();
    restore(repo.path(), &checkpoint.commit).unwrap();
    assert_ne!(fs::metadata(path).unwrap().permissions().mode() & 0o111, 0);
    assert_eq!(repo.index(), index);
}

#[test]
fn linked_worktree_uses_its_own_index_and_preserves_primary_checkout() {
    let repo = Repo::new();
    repo.write("tracked", b"primary\n");
    repo.commit();
    let temporary = tempfile::tempdir().unwrap();
    let linked = temporary.path().join("linked");
    checked(
        repo.path(),
        &[
            OsStr::new("worktree"),
            OsStr::new("add"),
            OsStr::new("-b"),
            OsStr::new("child"),
            linked.as_os_str(),
        ],
        None,
    )
    .unwrap();
    assert!(linked.join(".git").is_file());
    fs::write(linked.join("tracked"), b"linked snapshot\n").unwrap();
    let index_path = text(
        checked(
            &linked,
            &["rev-parse", "--path-format=absolute", "--git-path", "index"],
            None,
        )
        .unwrap(),
    )
    .unwrap();
    let index = fs::read(&index_path).unwrap();
    let checkpoint = create(&linked, "linked", CheckpointKind::Turn, "linked turn")
        .unwrap()
        .unwrap();
    fs::write(linked.join("tracked"), b"linked modified\n").unwrap();
    restore(&linked, &checkpoint.commit).unwrap();
    assert_eq!(
        fs::read(linked.join("tracked")).unwrap(),
        b"linked snapshot\n"
    );
    assert_eq!(fs::read(repo.path().join("tracked")).unwrap(), b"primary\n");
    assert_eq!(fs::read(index_path).unwrap(), index);
    assert_eq!(
        repo_root(&linked).unwrap(),
        fs::canonicalize(&linked).unwrap()
    );
    assert_eq!(list(repo.path(), "linked").unwrap(), [checkpoint]);
}

#[test]
fn unborn_head_and_missing_real_index_remain_unborn_and_missing() {
    let repo = Repo::new();
    repo.write("untracked", b"before first commit\n");
    assert!(repo.index().is_none());
    let checkpoint = repo.capture();
    assert_eq!(
        repo.git(&["rev-list", "--parents", "-n", "1", &checkpoint.commit])
            .split_whitespace()
            .count(),
        1
    );
    repo.write("untracked", b"later\n");
    repo.write("extra", b"remove\n");
    restore(repo.path(), &checkpoint.commit).unwrap();
    assert_eq!(
        fs::read(repo.path().join("untracked")).unwrap(),
        b"before first commit\n"
    );
    assert!(!repo.path().join("extra").exists());
    assert!(repo.index().is_none());
    assert!(
        !run(repo.path(), &["rev-parse", "--verify", "HEAD"], None)
            .unwrap()
            .ok()
    );
}

#[test]
fn changed_gitlinks_are_refused_but_unchanged_dirty_submodules_stay_untouched() {
    let repo = Repo::new();
    repo.git(&["init", "--initial-branch=main", "sub"]);
    let sub = repo.path().join("sub");
    repo.write("sub/file", b"submodule before\n");
    checked(&sub, &["add", "-A"], None).unwrap();
    checked(&sub, &["commit", "-m", "submodule first"], None).unwrap();
    repo.write("tracked", b"before\n");
    repo.commit();
    let checkpoint = repo.capture();
    repo.write("sub/file", b"dirty submodule\n");
    repo.write("tracked", b"after\n");
    restore(repo.path(), &checkpoint.commit).unwrap();
    assert_eq!(fs::read(sub.join("file")).unwrap(), b"dirty submodule\n");
    checked(&sub, &["add", "-A"], None).unwrap();
    checked(&sub, &["commit", "-m", "submodule changed"], None).unwrap();
    repo.write("tracked", b"must not be restored\n");
    let index = repo.index();
    assert!(matches!(
        restore(repo.path(), &checkpoint.commit),
        Err(CheckpointError::GitlinkChanged { .. })
    ));
    assert_eq!(
        fs::read(repo.path().join("tracked")).unwrap(),
        b"must not be restored\n"
    );
    assert_eq!(repo.index(), index);
}

#[test]
fn diff_stat_handles_binary_paths_with_tabs_and_rename_counts() {
    let repo = Repo::new();
    let name = if cfg!(unix) {
        "tab\tname"
    } else {
        "space name"
    };
    repo.write("binary", b"\0before");
    repo.write(name, b"one\n");
    repo.commit();
    let checkpoint = repo.capture();
    repo.write("binary", b"\0after");
    repo.write(name, b"one\ntwo\n");
    let deltas = diff_stat(repo.path(), &checkpoint.commit).unwrap();
    let binary = deltas.iter().find(|d| d.path == "binary").unwrap();
    assert_eq!((binary.added, binary.removed), (None, None));
    let tab = deltas.iter().find(|d| d.path == name).unwrap();
    assert_eq!((tab.added, tab.removed), (Some(1), Some(0)));
}

#[test]
fn invalid_session_ref_paths_are_rejected() {
    let repo = Repo::new();
    for id in ["", "../other", "nested/session", "bad\nname"] {
        assert!(matches!(
            list(repo.path(), id),
            Err(CheckpointError::InvalidSessionId)
        ));
        assert!(matches!(
            create(repo.path(), id, CheckpointKind::Turn, "x"),
            Err(CheckpointError::InvalidSessionId)
        ));
    }
}

#[test]
fn temporary_snapshots_clear_assume_unchanged_but_preserve_skip_worktree() {
    let repo = Repo::new();
    repo.write("tracked", b"initial\n");
    repo.write("sparse", b"sparse content\n");
    repo.commit();
    let baseline = repo.capture();
    repo.git(&["update-index", "--assume-unchanged", "tracked"]);
    repo.git(&["update-index", "--skip-worktree", "sparse"]);
    repo.git(&["update-index", "--assume-unchanged", "sparse"]);
    repo.write("tracked", b"changed despite the assume-unchanged flag\n");
    fs::remove_file(repo.path().join("sparse")).unwrap();
    let real_index = repo.index();
    let current = snapshot(repo.path()).unwrap();
    let flags =
        text(checked(repo.path(), &["ls-files", "-v"], Some(&current.index)).unwrap()).unwrap();
    assert!(flags.lines().any(|line| line == "H tracked"), "{flags}");
    assert!(flags.lines().any(|line| line == "S sparse"), "{flags}");
    assert_eq!(repo.index(), real_index);
    let deltas = diff_stat(repo.path(), &baseline.commit).unwrap();
    assert_eq!(deltas.len(), 1);
    assert_eq!(deltas[0].path, "tracked");
    let captured = repo.capture();
    assert_eq!(captured.seq, 2);
    assert_eq!(
        repo.git(&["show", &format!("{}:tracked", captured.commit)]),
        "changed despite the assume-unchanged flag"
    );
    assert_eq!(
        repo.git(&["show", &format!("{}:sparse", captured.commit)]),
        "sparse content"
    );
    assert_eq!(repo.index(), real_index);
    let real_flags = repo.git(&["ls-files", "-v"]);
    assert!(
        real_flags.lines().any(|line| line == "h tracked"),
        "{real_flags}"
    );
    assert!(
        real_flags.lines().any(|line| line == "s sparse"),
        "{real_flags}"
    );
}

#[cfg(unix)]
#[test]
fn restore_refuses_symlinked_parents_before_deleting_or_writing_anything() {
    use std::os::unix::fs::symlink;
    for in_checkpoint in [true, false] {
        let repo = Repo::new();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("file"), b"outside must survive\n").unwrap();
        repo.write("anchor", b"before checkpoint\n");
        if in_checkpoint {
            repo.write("dir/file", b"checkpoint file\n");
        }
        repo.commit();
        let checkpoint = repo.capture();
        if !in_checkpoint {
            repo.write("dir/file", b"indexed after checkpoint\n");
            repo.git(&["add", "dir/file"]);
        }
        repo.git(&["update-index", "--assume-unchanged", "dir/file"]);
        fs::remove_file(repo.path().join("dir/file")).unwrap();
        fs::remove_dir(repo.path().join("dir")).unwrap();
        repo.write(".gitignore", b"/dir\n");
        symlink(outside.path(), repo.path().join("dir")).unwrap();
        repo.write("anchor", b"current state must survive\n");
        let index = repo.index();
        assert!(matches!(
            restore(repo.path(), &checkpoint.commit),
            Err(CheckpointError::UnsafeParent { path }) if path == repo.path().join("dir")
        ));
        assert_eq!(
            fs::read(outside.path().join("file")).unwrap(),
            b"outside must survive\n"
        );
        assert_eq!(
            fs::read(repo.path().join("anchor")).unwrap(),
            b"current state must survive\n"
        );
        assert_eq!(
            fs::read_link(repo.path().join("dir")).unwrap(),
            outside.path()
        );
        assert_eq!(repo.index(), index);
    }
}

#[test]
fn ignored_overlap_comparison_folds_case_component_by_component() {
    for (left, right) in [
        ("Foo", "foo"),
        ("Foo/child", "foo"),
        ("Foo", "foo/child"),
        ("PARENT/Σ", "parent/ς/child"),
    ] {
        assert!(
            paths_overlap(Path::new(left), Path::new(right), true),
            "{left} vs {right}"
        );
        assert!(
            !paths_overlap(Path::new(left), Path::new(right), false),
            "{left} vs {right}"
        );
    }
    for (left, right) in [("Foo", "foobar"), ("dir/a", "DIR/b"), ("a/b", "a/bb")] {
        assert!(
            !paths_overlap(Path::new(left), Path::new(right), true),
            "{left} vs {right}"
        );
    }
    assert!(paths_overlap(
        Path::new("same/path"),
        Path::new("same"),
        false
    ));
}

#[cfg(windows)]
#[test]
fn ignored_case_collision_is_refused_before_touching_the_working_tree() {
    let repo = Repo::new();
    repo.write("Foo", b"checkpoint content\n");
    repo.write("anchor", b"before\n");
    repo.commit();
    let checkpoint = repo.capture();
    repo.git(&["rm", "--cached", "Foo"]);
    fs::remove_file(repo.path().join("Foo")).unwrap();
    repo.write(".gitignore", b"/foo\n");
    repo.write("foo", b"ignored must survive\n");
    repo.write("anchor", b"current anchor\n");
    let index = repo.index();
    assert!(matches!(
        restore(repo.path(), &checkpoint.commit),
        Err(CheckpointError::IgnoredCollision { .. })
    ));
    assert_eq!(
        fs::read(repo.path().join("foo")).unwrap(),
        b"ignored must survive\n"
    );
    assert_eq!(
        fs::read(repo.path().join("anchor")).unwrap(),
        b"current anchor\n"
    );
    assert_eq!(repo.index(), index);
}
