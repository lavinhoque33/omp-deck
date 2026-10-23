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
