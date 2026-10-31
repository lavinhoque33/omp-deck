use std::fs;

use deck_core::folders::list_dirs;

#[test]
fn lists_visible_directories_sorted_with_repo_flag() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path();
    for dir in ["beta", "Alpha", "gamma", ".hidden", "repo/.git", "wt"] {
        fs::create_dir_all(p.join(dir)).unwrap();
    }
    fs::write(p.join("wt/.git"), "gitdir: /x").unwrap(); // linked worktree: .git is a file
    fs::write(p.join("file.txt"), "x").unwrap();

    let l = list_dirs(p, 100).unwrap();
    assert!(!l.truncated);
    let got: Vec<(&str, bool)> = l
        .entries
        .iter()
        .map(|e| (e.name.as_str(), e.repo))
        .collect();
    assert_eq!(
        got,
        [
            ("Alpha", false),
            ("beta", false),
            ("gamma", false),
            ("repo", true),
            ("wt", true)
        ]
    );
    assert_eq!(l.entries[0].path, p.join("Alpha"));
}

#[test]
fn truncates_at_the_limit_in_sort_order() {
    let d = tempfile::tempdir().unwrap();
    for n in ["c", "a", "b"] {
        fs::create_dir(d.path().join(n)).unwrap();
    }
    let l = list_dirs(d.path(), 2).unwrap();
    assert!(l.truncated);
    assert_eq!(
        l.entries
            .iter()
            .map(|e| e.name.as_str())
            .collect::<Vec<_>>(),
        ["a", "b"]
    );
    assert!(!list_dirs(d.path(), 3).unwrap().truncated);
    assert!(list_dirs(&d.path().join("missing"), 3).is_err());
}

#[cfg(unix)]
#[test]
fn symlinks_to_directories_are_listed_and_broken_ones_skipped() {
    use std::os::unix::fs::symlink;
    let d = tempfile::tempdir().unwrap();
    let p = d.path();
    fs::create_dir_all(p.join("real/.git")).unwrap();
    fs::write(p.join("f"), "x").unwrap();
    symlink(p.join("real"), p.join("link")).unwrap();
    symlink(p.join("gone"), p.join("broken")).unwrap();
    symlink(p.join("f"), p.join("tofile")).unwrap();
    let l = list_dirs(p, 10).unwrap();
    let got: Vec<(&str, bool)> = l
        .entries
        .iter()
        .map(|e| (e.name.as_str(), e.repo))
        .collect();
    assert_eq!(got, [("link", true), ("real", true)]);
}
