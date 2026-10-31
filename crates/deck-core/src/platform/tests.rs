use std::collections::HashMap;
use std::fs::{self, File};
use std::path::PathBuf;

use super::*;

fn touch(dir: &Path, name: &str) -> PathBuf {
    let p = dir.join(name);
    fs::write(&p, "x").unwrap();
    p
}

fn exts(list: &[&str]) -> Vec<String> {
    list.iter().map(|e| (*e).to_owned()).collect()
}

#[test]
fn find_in_tries_each_extension_in_order_inside_the_first_dir_that_has_one() {
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    touch(a.path(), "omp.cmd");
    touch(b.path(), "omp.exe");
    let dirs = || vec![a.path().to_owned(), b.path().to_owned()];
    // Directory order wins over extension order.
    let hit = find_in(dirs(), "omp", &exts(&[".exe", ".cmd"]), Path::is_file);
    assert_eq!(hit, Some(a.path().join("omp.cmd")));
    // Inside one directory the earlier extension wins.
    touch(a.path(), "omp.exe");
    let hit = find_in(dirs(), "omp", &exts(&[".exe", ".cmd"]), Path::is_file);
    assert_eq!(hit, Some(a.path().join("omp.exe")));
    assert_eq!(
        find_in(dirs(), "nope", &exts(&[".exe"]), Path::is_file),
        None
    );
}

#[test]
fn find_in_uses_a_name_that_already_carries_a_listed_extension_as_is() {
    let d = tempfile::tempdir().unwrap();
    touch(d.path(), "omp.CMD");
    touch(d.path(), "omp.CMD.exe");
    let hit = find_in(
        [d.path().to_owned()],
        "omp.CMD",
        &exts(&[".exe", ".cmd"]),
        Path::is_file,
    );
    assert_eq!(hit, Some(d.path().join("omp.CMD")));
    // An unlisted extension is a part of the base name: extensions are appended.
    let hit = find_in(
        [d.path().to_owned()],
        "omp.CMD",
        &exts(&[".exe"]),
        Path::is_file,
    );
    assert_eq!(hit, Some(d.path().join("omp.CMD.exe")));
}

#[test]
fn find_in_without_extensions_matches_the_exact_name() {
    let d = tempfile::tempdir().unwrap();
    touch(d.path(), "omp");
    touch(d.path(), "omp.exe");
    assert_eq!(
        find_in([d.path().to_owned()], "omp", &[], Path::is_file),
        Some(d.path().join("omp"))
    );
}

#[cfg(unix)]
mod unix {
    use std::io::Read;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::os::unix::process::ExitStatusExt;
    use std::process::Stdio;

    use super::*;

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn is_executable_needs_an_execute_bit_on_a_regular_file() {
        let d = tempfile::tempdir().unwrap();
        let plain = touch(d.path(), "plain");
        assert!(!is_executable(&plain));
        fs::set_permissions(&plain, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(is_executable(&plain));
        let dir = d.path().join("dir");
        fs::create_dir(&dir).unwrap();
        assert!(!is_executable(&dir), "a directory is not a program");
        assert!(!is_executable(&d.path().join("missing")));
    }

    #[test]
    fn private_files_and_dirs_are_owner_only_and_never_overwrite() {
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().join("a/b");
        create_private_dir_all(&dir).unwrap();
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&d.path().join("a")), 0o700);
        let one = d.path().join("one");
        create_private_dir(&one).unwrap();
        assert_eq!(mode(&one), 0o700);
        assert!(create_private_dir(&one).is_err());

        let f = dir.join("f");
        open_private_file(&f).unwrap();
        assert_eq!(mode(&f), 0o600);
        let err = open_private_file(&f).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        let link = dir.join("link");
        symlink(d.path().join("target"), &link).unwrap();
        assert!(
            open_private_file(&link).is_err(),
            "create_new never follows a link"
        );
        assert!(!d.path().join("target").exists());
    }

    #[test]
    fn write_private_replaces_atomically_and_leaves_no_temp_file() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("deep/dir/secret");
        write_private(&f, b"one").unwrap();
        assert_eq!(fs::read(&f).unwrap(), b"one");
        assert_eq!(mode(&f), 0o600);
        write_private(&f, b"two").unwrap();
        assert_eq!(fs::read(&f).unwrap(), b"two");
        let names: Vec<_> = fs::read_dir(f.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["secret"]);
    }

    #[test]
    fn find_in_with_the_executable_check_skips_non_executables() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        touch(a.path(), "tool");
        let want = touch(b.path(), "tool");
        fs::set_permissions(&want, fs::Permissions::from_mode(0o755)).unwrap();
        let dirs = vec![a.path().to_owned(), b.path().to_owned()];
        assert_eq!(find_in(dirs, "tool", &[], is_executable), Some(want));
    }

    fn pid_alive(pid: i32) -> bool {
        // SAFETY: signal 0 only checks that the process exists.
        unsafe { libc::kill(pid, 0) == 0 }
    }

    #[test]
    fn killing_the_tree_takes_down_a_backgrounded_grandchild() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "sleep 30 & echo $!; wait"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = spawn_killable(cmd).unwrap();
        let mut out = child.stdout.take().unwrap();
        let mut line = Vec::new();
        let mut byte = [0u8; 1];
        while out.read(&mut byte).unwrap() == 1 && byte[0] != b'\n' {
            line.push(byte[0]);
        }
        let grandchild: i32 = String::from_utf8(line).unwrap().trim().parse().unwrap();
        assert!(pid_alive(grandchild));
        child.kill_tree();
        let status = child.wait().unwrap();
        assert_eq!(status.signal(), Some(libc::SIGKILL));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        // The orphaned grandchild is reaped by init; wait until it is gone.
        while pid_alive(grandchild) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(!pid_alive(grandchild), "grandchild survived kill_tree");
    }

    #[test]
    fn dropping_a_killable_child_kills_its_tree() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "sleep 30 & echo $!; wait"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = spawn_killable(cmd).unwrap();
        let mut out = child.stdout.take().unwrap();
        let mut line = Vec::new();
        let mut byte = [0u8; 1];
        while out.read(&mut byte).unwrap() == 1 && byte[0] != b'\n' {
            line.push(byte[0]);
        }
        let grandchild: i32 = String::from_utf8(line).unwrap().trim().parse().unwrap();
        drop(child);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while pid_alive(grandchild) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(!pid_alive(grandchild));
    }

    #[test]
    fn open_regular_file_refuses_links_and_fifos_but_opens_files() {
        let d = tempfile::tempdir().unwrap();
        let f = touch(d.path(), "f");
        assert!(open_regular_file(&f).is_ok());
        let link = d.path().join("l");
        symlink(&f, &link).unwrap();
        assert!(open_regular_file(&link).is_err());
        let fifo = d.path().join("fifo");
        // SAFETY: the path is a valid NUL-terminated string for the call's duration.
        let c = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        assert!(open_regular_file(&fifo).is_err());
        assert!(open_regular_file(&d.path().join("missing")).is_err());
    }

    #[test]
    fn process_start_is_none_for_a_missing_pid_and_not_in_the_future_for_ours() {
        if !cfg!(target_os = "linux") {
            return;
        }
        let started = process_start(std::process::id()).unwrap();
        assert!(started <= SystemTime::now());
        assert_eq!(process_start(u32::MAX - 1), None);
    }
}

#[test]
fn file_identity_follows_the_file_not_the_name() {
    let d = tempfile::tempdir().unwrap();
    let a = touch(d.path(), "a");
    let b = touch(d.path(), "b");
    let id = |p: &Path| file_identity(&File::open(p).unwrap()).unwrap();
    assert_eq!(id(&a), id(&a));
    assert_ne!(id(&a), id(&b));
    let moved = d.path().join("moved");
    fs::rename(&a, &moved).unwrap();
    let before = id(&b);
    assert_eq!(id(&moved), id(&moved));
    assert_eq!(before, id(&b));
    // A replacement under the old name is a different file.
    let a2 = touch(d.path(), "a");
    assert_ne!(id(&a2), id(&moved));
}

#[test]
fn path_bytes_round_trip_utf8_names() {
    assert_eq!(path_bytes(Path::new("/a/ü.jsonl")), "/a/ü.jsonl".as_bytes());
}

fn table(edges: &[(u32, &[u32])]) -> HashMap<u32, Vec<u32>> {
    edges.iter().map(|(p, kids)| (*p, kids.to_vec())).collect()
}

#[test]
fn tree_aggregation_sums_root_and_descendants_per_root() {
    // 1 ─ 2 ─ 4, 1 ─ 3; 10 is a separate tree; 99 is an unrelated process.
    let children = table(&[(1, &[2, 3]), (2, &[4]), (10, &[])]);
    let mem = |pid: u32| Some(u64::from(pid) * 100);
    let got = aggregate_tree(&[1, 2, 10], &children, mem);
    assert_eq!(
        got,
        vec![
            Some(TreeMemory {
                bytes: 1000,
                processes: 4
            }),
            Some(TreeMemory {
                bytes: 600,
                processes: 2
            }),
            Some(TreeMemory {
                bytes: 1000,
                processes: 1
            }),
        ]
    );
}

#[test]
fn tree_aggregation_survives_cycles_duplicates_and_vanished_processes() {
    // pid reuse can make a table cyclic (2 → 1 → 2) and list a child twice; 5 vanished between the
    // table pass and its memory read; the root 7 is gone altogether.
    let children = table(&[(1, &[2, 2, 5]), (2, &[1, 3])]);
    let mem = |pid: u32| (pid != 5 && pid != 7).then_some(10);
    let got = aggregate_tree(&[1, 7], &children, mem);
    assert_eq!(
        got,
        vec![
            Some(TreeMemory {
                bytes: 30,
                processes: 3
            }),
            None
        ]
    );
}

#[test]
fn tree_aggregation_of_nothing_is_empty() {
    assert!(tree_memory(&[]).is_empty());
}

/// A shell with one background sleeper is two processes; the shell's tree accounts for both and
/// a pid that does not exist is `None`.
#[cfg(target_os = "linux")]
#[test]
fn tree_memory_counts_a_spawned_child_tree() {
    struct Reap(std::process::Child);
    impl Drop for Reap {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let child = Command::new("sh")
        .args(["-c", "sleep 30 & wait"])
        .spawn()
        .unwrap();
    let root = child.id();
    let _reap = Reap(child);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let tree = loop {
        let tree = tree_memory(&[root, u32::MAX]);
        if tree[0].is_some_and(|t| t.processes >= 2) {
            break tree;
        }
        assert!(std::time::Instant::now() < deadline, "no sleeper: {tree:?}");
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    let own = tree[0].unwrap();
    assert!(own.bytes > 0, "{own:?}");
    assert_eq!(tree[1], None);
    let shell = process_memory(root).unwrap();
    assert_eq!(shell.processes, 1);
    assert!(own.bytes > shell.bytes, "{own:?} vs {shell:?}");
}

#[cfg(windows)]
#[test]
fn verbatim_prefix_is_dropped_only_for_drive_and_unc_paths() {
    let plain = |s: &str| strip_verbatim(PathBuf::from(s));
    assert_eq!(plain(r"\\?\C:\x\y"), PathBuf::from(r"C:\x\y"));
    assert_eq!(
        plain(r"\\?\UNC\srv\share\x"),
        PathBuf::from(r"\\srv\share\x")
    );
    assert_eq!(plain(r"C:\x"), PathBuf::from(r"C:\x"));
    assert_eq!(plain(r"\\srv\share"), PathBuf::from(r"\\srv\share"));
    assert_eq!(plain(r"\\?\Volume{1}\x"), PathBuf::from(r"\\?\Volume{1}\x"));
}

#[cfg(unix)]
#[test]
fn secret_dirs_are_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let d = tempfile::tempdir().unwrap();
    let dir = d.path().join("a/b");
    create_secret_dir_all(&dir).unwrap();
    let mode = |p: &std::path::Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&dir), 0o700);
    // An existing, looser dir of this user is tightened.
    let loose = d.path().join("loose");
    fs::create_dir(&loose).unwrap();
    fs::set_permissions(&loose, fs::Permissions::from_mode(0o755)).unwrap();
    create_secret_dir_all(&loose).unwrap();
    assert_eq!(mode(&loose), 0o700);
    // A file in its place is no folder.
    let file = d.path().join("file");
    fs::write(&file, b"").unwrap();
    assert!(create_secret_dir_all(&file).is_err());
}

/// Runs only on Windows (CI's windows job); on Linux it is compiled by the xwin clippy check.
/// A fresh and an already existing (inheriting) directory both end with a protected DACL of
/// exactly SYSTEM + the current user, which files created inside inherit. Trustees are compared
/// as SIDs: SDDL text prints well-known accounts as aliases (CI's built-in Administrator is `LA`).
#[cfg(windows)]
#[test]
fn secret_dirs_get_a_protected_user_and_system_dacl() {
    use windows_sys::Win32::Security::{CONTAINER_INHERIT_ACE, INHERITED_ACE, OBJECT_INHERIT_ACE};
    use windows_sys::Win32::Storage::FileSystem::FILE_ALL_ACCESS;
    const SYSTEM: &str = "S-1-5-18";
    let user = imp::current_sid_string().expect("current user's SID");
    let mut want = vec![SYSTEM, user.as_str()];
    want.sort_unstable();
    // Every ACE allows full control with `flags`; together they name exactly SYSTEM + the user.
    let check = |path: &std::path::Path, flags: u32| {
        let dacl = imp::dacl_of(path).unwrap();
        let ctx = format!("{}: {}", path.display(), dacl.sddl);
        let mut sids: Vec<&str> = dacl.aces.iter().map(|a| a.sid.as_str()).collect();
        sids.sort_unstable();
        assert_eq!(sids, want, "{ctx}");
        for ace in &dacl.aces {
            assert!(ace.allow, "{ctx}");
            assert_eq!(ace.mask, FILE_ALL_ACCESS, "{ctx}");
            assert_eq!(u32::from(ace.flags), flags, "{ctx}");
        }
        dacl.protected
    };
    let d = tempfile::tempdir().unwrap();
    let fresh = d.path().join("a/secrets");
    let existing = d.path().join("existing");
    fs::create_dir(&existing).unwrap();
    for dir in [&fresh, &existing] {
        create_secret_dir_all(dir).unwrap();
        assert!(
            check(dir, OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE),
            "{}: DACL not protected",
            dir.display()
        );
        let file = dir.join("remote.key");
        write_private(&file, b"k").unwrap();
        check(&file, INHERITED_ACE);
    }
}
