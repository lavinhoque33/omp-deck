use std::fs;
use std::sync::mpsc;
use std::time::Duration;

use super::*;

/// `Bun.hash(Uint8Array)` of the first n bytes of the pattern `(i * 31 + 7) mod 256`, n = 0..=130
/// (every branch of the algorithm: empty, 1-3, 4-16, 17-48, and the 48-byte rounds), from
/// `bun` 1.4.2 (`/tmp` script: `for n in 0..=130 { Bun.hash(bytes).toString() }`).
const PATTERN: [u64; 131] = [
    0x0409638ee2bde459,
    0xfddeeeea8cc2709c,
    0xca53c1e8890f395c,
    0xaa4dada6d17eebb0,
    0x8d9d4657e96cc294,
    0xc828203113ad94a7,
    0xa75808396fd75dd3,
    0x935a88298d5507c9,
    0x9654832f28858268,
    0x708e1187f06d6aaa,
    0x77fa44f8dd12a051,
    0xc7c5f909cb3d85f0,
    0x1318f6e5e82d7b82,
    0x4fb03fd81ac89239,
    0x8cebe76fa1b17ab5,
    0xb10ea23d7453482e,
    0x36b53f8551944db0,
    0x904849bdd1e93c7c,
    0x0d98ef4a3c92af96,
    0xca2d15782505311f,
    0x32050cd8c61dc18d,
    0x662bddf5ab9a28c3,
    0x02774de5eda3b132,
    0x9a8f140c6e2806b1,
    0xdcbb665031fbe540,
    0x7a668da7d82f7060,
    0x792cb05b955b027b,
    0xbac4b930a3eeca75,
    0xa1acef22de70096f,
    0xa8f883838987f661,
    0x086a98889b7cd01d,
    0x21008267c30eb404,
    0x4d1435b5e345c9cb,
    0x35eceb8164dee4ae,
    0x4899ad041854896e,
    0xf1dc46665a39a2ec,
    0xdecc02217fd63d71,
    0x895c1fc05d9dc8d0,
    0x4be97101cb6a7ce8,
    0xe20d0ed0d11b1f7c,
    0x5b3a4d5934744cc8,
    0x8c2ab8137d27ff91,
    0x3bc771bde6592cb2,
    0x4e0ba983706d0298,
    0x5bddc9b2ab2e913a,
    0xdbb81596d4c0b83f,
    0xbd0e9fbcbe8c083d,
    0x5dc5aaaf19d7cfc9,
    0x3ec1b034dbe02bd7,
    0x30161cb91c8df53e,
    0x0468d15de2b1c45b,
    0x70603edbc35104d8,
    0x2882ab2d1ee9b643,
    0xd8eebe5d92a25d17,
    0xa9322c5cad0ea2c2,
    0xe79f67c1c097c5d8,
    0xe3b8b8733b576b03,
    0x25a47bd2221d7deb,
    0x75b9b26b3a37cfbf,
    0xb8f375f9cb89de59,
    0xa9b3477f9b5e7d53,
    0x84dc2b1cb447b89a,
    0x0fb92c8513b158c3,
    0x0794c5de59c19d99,
    0xefd3e3780f38a91c,
    0x54e9e0f63adbfdef,
    0xe28ab972c511dedf,
    0x47143e50f6f13eaf,
    0xf2e0d121f7852d6f,
    0xf26a913bd817eadd,
    0xf9c3432e96f60ba3,
    0xcc8594f1a3e49ae6,
    0x3923c862de96650e,
    0x843b31c1d69ccf72,
    0x587b499edc8ac33e,
    0x947efc8445d22dfd,
    0x235b648bb15e3a42,
    0xd85f92dcacdd3fc0,
    0xc974d8edc30fbad3,
    0xc3d1b25d9d82de83,
    0x4e66c8e442609ccc,
    0x19b367559c833dbf,
    0x9d4623776b56a3af,
    0xae1df5914a61bc27,
    0x82ed4c75e45659f7,
    0xb76a0b2e5366bfdf,
    0xe64c141e7b35f114,
    0x962fb111d187a6ad,
    0xa9997e31101c84a3,
    0xb7bb67b7df0b81fc,
    0xfeec3705d6756a28,
    0x6608001f521f4436,
    0x96cfd1aecb5e0779,
    0x97c26c6c403551cc,
    0xc2d37040bb6bf59a,
    0x8433907cbfd6e01a,
    0xaafaa5ff0421ff8c,
    0x987658679c36fd56,
    0x3d46f355b7ca9062,
    0x7a63d4c10f8aa776,
    0xe0c3d79ee1609ba9,
    0x0ae4bb38961e1da8,
    0x8ddab4391debf7bb,
    0x134d493bcf743ac2,
    0xa23f9daef1e73ef8,
    0x0a8dfb6ca4584496,
    0xa895341a91154fb4,
    0xb968f2d751d29b8c,
    0xa9705b8dd8da7a88,
    0xd6be872cec2c0ec3,
    0xc38a273fefcba3e6,
    0x709c766e8a60c1de,
    0xc00e00a40912c7fb,
    0x4677bb53fcda2ef2,
    0x1b30f621a1a7f8b4,
    0x971baa7940d3d37c,
    0x1eedaac44c49b875,
    0xf6178f0136b84c60,
    0x6dd9544494255de4,
    0xd9c53c4455363e7a,
    0xe827b6d77f0be1ce,
    0x8b72bf8aa1914658,
    0x595fb61be40d8cba,
    0xef92c47cc335a6f3,
    0x4dd0218270a9be9c,
    0x488f72e237a3ed74,
    0x7595333d758900fe,
    0xeefe7c161f11d539,
    0x4329d1a474b869f6,
    0x53ccfa01fcbb2523,
    0x79a7b93239582472,
];

fn pattern(n: usize) -> Vec<u8> {
    (0..n).map(|i| ((i * 31 + 7) % 256) as u8).collect()
}

#[test]
fn wyhash_matches_bun_for_every_length_and_branch() {
    for (n, want) in PATTERN.iter().enumerate() {
        assert_eq!(wyhash(&pattern(n)), *want, "length {n}");
    }
}

#[test]
fn wyhash_hashes_utf8_bytes_like_bun_hash_of_a_string() {
    // `Bun.hash("é")` etc. (decimal, as bun prints the bigint).
    for (s, want) in [
        ("é", 1_783_465_187_472_633_034u64),
        ("日本語", 9_333_711_280_020_391_303),
        ("a😀b", 8_538_141_982_363_079_751),
    ] {
        assert_eq!(wyhash(s.as_bytes()), want, "{s}");
    }
}

#[test]
fn base36_is_lowercase_and_unpadded() {
    assert_eq!(base36(0), "0");
    assert_eq!(base36(35), "z");
    assert_eq!(base36(36), "10");
    assert_eq!(base36(u64::MAX), "3w5e11264sgsf");
}

/// (input path, expected bank id), computed in bun by `projectBankSegment(path.posix.resolve(p))`
/// replicating `mnemopi/config.ts:206-292`.
#[cfg(unix)]
const POSIX_BANKS: [(&str, &str); 15] = [
    (
        "/home/axeleratorx/PersProject/OmpDeck",
        "OmpDeck-2k8vi2qa3ocpc",
    ),
    (
        "/home/axeleratorx/PersProject/Samsung Playground",
        "Samsung-Playground-1zd7iwe9bjpld",
    ),
    // Empty basename → "default".
    ("/", "default-c8rgibd8h0ws"),
    // Trailing separator and `.`/`..` segments resolve away.
    (
        "/home/axeleratorx/PersProject/OmpDeck/",
        "OmpDeck-2k8vi2qa3ocpc",
    ),
    (
        "/home/axeleratorx/PersProject/./x/../OmpDeck",
        "OmpDeck-2k8vi2qa3ocpc",
    ),
    // `..` past the root stays at the root.
    ("/a/b/../../../..", "default-c8rgibd8h0ws"),
    // Non-ASCII characters become dashes; the hash covers the real UTF-8 path.
    ("/home/u/café au lait", "caf-au-lait-fekmft55mkd9"),
    ("/home/u/a😀b", "a-b-1cijb5nn4up03"),
    // Sanitizes to nothing → "default".
    ("/tmp/日本語", "default-2k82y39m9grgk"),
    ("/home/u/Проект-é", "default-2b4353glcfuq9"),
    ("/home/u/ spaced  name ", "spaced-name-2o6kuuprd4jwo"),
    ("/home/u/-dash-", "dash-2qcnay490pmdf"),
    // 70-character name: shortened with its own hash, then the segment fits.
    (
        "/work/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-9gqd3frwwed3",
    ),
    // The cut lands inside a run of dashes: they are stripped from the prefix.
    (
        "/work/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa--bbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-2agri7yqnx1fd",
    ),
    // Name fits (57) but `<name>-<hash>` does not: the segment is shortened.
    (
        "/w/xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx_y",
        "xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx-x4nxoxjlt47l",
    ),
];

#[cfg(unix)]
#[test]
fn bank_ids_match_omp_for_posix_paths() {
    for (path, want) in POSIX_BANKS {
        assert_eq!(bank_id(Path::new(path)), want, "{path}");
        assert!(want.len() <= MAX_NAME);
    }
}

/// `path.win32.resolve` + the same segment logic, from bun.
#[cfg(windows)]
#[test]
fn bank_ids_match_omp_for_windows_paths() {
    for (path, want) in [
        ("C:\\Users\\x\\proj", "proj-2kzkx7ckygtak"),
        ("C:\\Users\\x\\..\\y\\Some Proj\\", "Some-Proj-hvtz1i8m4xiy"),
        ("C:\\", "default-27skzdezktm"),
        ("D:\\a\\.\\b\\日本語\\c.d", "c-d-2sbse6fkv3nwl"),
    ] {
        assert_eq!(bank_id(Path::new(path)), want, "{path}");
    }
}

/// `path.win32.resolve` + `basename` + the same segment logic, from bun (verbatim inputs are
/// compared as the `process.cwd()` string omp sees: `\\?\` stripped).
#[cfg(windows)]
#[test]
fn bank_ids_match_omp_for_unc_and_verbatim_windows_paths() {
    for (path, want) in [
        (r"\\server\share", "share-3pucqe41inogi"),
        (r"\\server\share\", "share-3pucqe41inogi"),
        (r"\\server\share\proj", "proj-3fh7dd6fbd2au"),
        (r"\\?\C:\foo\..\bar", "bar-3hdf9perf1hgs"),
        (r"\\?\UNC\server\share\foo\..\bar", "bar-2jwopcx82d2qp"),
        (r"\\?\UNC\server\share", "share-3pucqe41inogi"),
    ] {
        assert_eq!(bank_id(Path::new(path)), want, "{path}");
    }
}

/// `C:foo` is relative to the current directory of drive `C:`; the process cwd is on one drive.
#[cfg(windows)]
#[test]
fn a_drive_relative_path_resolves_against_that_drives_directory() {
    let cwd = std::env::current_dir().unwrap();
    let cwd = cwd.to_string_lossy().into_owned();
    let drive = &cwd[..2];
    assert_eq!(
        bank_id(Path::new(&format!("{drive}proj"))),
        bank_id(&Path::new(&cwd).join("proj"))
    );
}

#[test]
fn verbatim_prefixes_are_only_stripped_from_whole_prefixes() {
    assert_eq!(strip_verbatim(r"\\?\C:\x").as_deref(), Some(r"C:\x"));
    assert_eq!(
        strip_verbatim(r"\\?\UNC\server\share\x").as_deref(),
        Some(r"\\server\share\x")
    );
    assert_eq!(strip_verbatim(r"\\?\Volume{1}\x"), None);
    assert_eq!(strip_verbatim(r"C:\x"), None);
}

fn env(home: &Path, xdg: Option<&Path>) -> Env {
    Env {
        home: home.to_owned(),
        config_dir: None,
        xdg_state_home: xdg.map(|p| p.as_os_str().to_owned()),
        xdg_platform: true,
    }
}

#[test]
fn memories_follow_xdg_state_only_for_the_default_agent_dir_when_it_exists() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let state = tmp.path().join("state");
    let default_agent = home.join(".omp/agent");
    let in_agent = |a: &Path| a.join("memories/sharpshooter");
    let in_xdg = state.join("omp/memories/sharpshooter");

    // XDG_STATE_HOME/omp does not exist yet: omp stays in the agent dir.
    assert_eq!(
        root_dir_in(&default_agent, &env(&home, Some(&state))),
        in_agent(&default_agent)
    );
    fs::create_dir_all(state.join("omp")).unwrap();
    assert_eq!(
        root_dir_in(&default_agent, &env(&home, Some(&state))),
        in_xdg
    );
    // Not set → agent dir.
    assert_eq!(
        root_dir_in(&default_agent, &env(&home, None)),
        in_agent(&default_agent)
    );
    // An agent dir somewhere else (PI_CODING_AGENT_DIR) is not the default profile.
    let custom = tmp.path().join("custom-agent");
    assert_eq!(
        root_dir_in(&custom, &env(&home, Some(&state))),
        in_agent(&custom)
    );
    // Windows ignores XDG.
    let mut windows_like = env(&home, Some(&state));
    windows_like.xdg_platform = false;
    assert_eq!(
        root_dir_in(&default_agent, &windows_like),
        in_agent(&default_agent)
    );
    // PI_CONFIG_DIR moves the default agent dir.
    let mut moved = env(&home, Some(&state));
    moved.config_dir = Some(".omp-work".into());
    assert_eq!(
        root_dir_in(&default_agent, &moved),
        in_agent(&default_agent)
    );
    let work_agent = home.join(".omp-work/agent");
    assert_eq!(root_dir_in(&work_agent, &moved), in_xdg);
    // omp resolves the configured directory lexically before comparing it with the default.
    for spelled in [
        home.join(".omp/agent/"),
        home.join(".omp/./agent"),
        home.join(".omp/x/../agent"),
    ] {
        assert_eq!(
            root_dir_in(&spelled, &env(&home, Some(&state))),
            in_xdg,
            "{}",
            spelled.display()
        );
    }
}

#[test]
fn bank_dir_is_the_root_dir_of_the_agent_plus_the_bank_id() {
    let tmp = tempfile::tempdir().unwrap();
    let root = SessionsRoot::at(tmp.path().join("agent/sessions"));
    let project = tmp.path().join("my proj");
    assert_eq!(
        bank_dir(&root, &project),
        tmp.path()
            .join("agent/memories/sharpshooter")
            .join(bank_id(&project))
    );
}

/// `Bun.hash.xxHash64(path, seed)` for omp's two seeds, joined as `memory_lock_name` does.
#[cfg(unix)]
#[test]
fn lock_names_match_omp() {
    let path = "/home/u/.omp/agent/memories/sharpshooter/proj-abc/consolidate.lock";
    assert_eq!(
        lock_name(path),
        "omp-file-lock-5287f9e67dac5397edf47fb4967416dd"
    );
    // omp locks `path.resolve(<bank>/consolidate) + ".lock"`.
    for bank in [
        "/home/u/.omp/agent/memories/sharpshooter/proj-abc",
        "/home/u/.omp/agent/memories/sharpshooter/proj-abc/",
        "/home/u/.omp/agent/memories/sharpshooter/./x/../proj-abc",
    ] {
        assert_eq!(lock_path(Path::new(bank)), path, "{bank}");
    }
}

#[test]
fn the_bank_lock_is_exclusive_until_dropped() {
    let tmp = tempfile::tempdir().unwrap();
    let bank = tmp.path().join("bank");
    fs::create_dir_all(&bank).unwrap();
    let first = lock_bank(&bank, Duration::ZERO).unwrap().expect("free");
    assert!(lock_bank(&bank, Duration::ZERO).unwrap().is_none());
    // Another bank is independent.
    let other = tmp.path().join("other");
    fs::create_dir_all(&other).unwrap();
    assert!(lock_bank(&other, Duration::ZERO).unwrap().is_some());
    drop(first);
    assert!(lock_bank(&bank, Duration::ZERO).unwrap().is_some());
}

#[test]
fn waiting_for_the_lock_gets_it_once_the_holder_lets_go() {
    let tmp = tempfile::tempdir().unwrap();
    let bank = tmp.path().join("bank");
    fs::create_dir_all(&bank).unwrap();
    let held = lock_bank(&bank, Duration::ZERO).unwrap().unwrap();
    let (tx, rx) = mpsc::channel();
    let holder = thread::spawn(move || {
        rx.recv().unwrap();
        thread::sleep(Duration::from_millis(250));
        drop(held);
    });
    tx.send(()).unwrap();
    let started = Instant::now();
    let got = lock_bank(&bank, Duration::from_secs(10)).unwrap();
    assert!(got.is_some(), "the lock was released while we waited");
    assert!(started.elapsed() < Duration::from_secs(5));
    holder.join().unwrap();

    // A holder that never lets go makes the wait end after about `wait`.
    let started = Instant::now();
    assert!(
        lock_bank(&bank, Duration::from_millis(300))
            .unwrap()
            .is_none()
    );
    assert!(started.elapsed() >= Duration::from_millis(300));
    drop(got);
}

/// On Linux the lock is exactly omp's abstract socket: binding that name by hand collides.
#[cfg(target_os = "linux")]
#[test]
fn the_linux_lock_is_omps_abstract_socket() {
    use std::os::linux::net::SocketAddrExt;
    use std::os::unix::net::{SocketAddr, UnixDatagram};

    let tmp = tempfile::tempdir().unwrap();
    let bank = tmp.path().join("bank");
    let name = lock_name(&format!("{}/consolidate.lock", bank.display()));
    let addr = SocketAddr::from_abstract_name(name.as_bytes()).unwrap();

    // Held by "omp" (a socket we bind ourselves): lock_bank reports it as held.
    let omp = UnixDatagram::bind_addr(&addr).unwrap();
    assert!(lock_bank(&bank, Duration::ZERO).unwrap().is_none());
    drop(omp);

    // Held by us: omp's bind (same name) would fail with AddrInUse.
    let ours = lock_bank(&bank, Duration::ZERO).unwrap().unwrap();
    let e = UnixDatagram::bind_addr(&addr).unwrap_err();
    assert_eq!(e.kind(), io::ErrorKind::AddrInUse);
    drop(ours);
    assert!(UnixDatagram::bind_addr(&addr).is_ok());
}

/// Without abstract sockets the lock is an `flock` on the lock file, which omp would also take.
#[cfg(all(unix, not(target_os = "linux")))]
#[test]
fn the_flock_lock_lives_in_the_bank_folder() {
    let tmp = tempfile::tempdir().unwrap();
    let bank = tmp.path().join("deep/bank");
    let held = lock_bank(&bank, Duration::ZERO).unwrap().unwrap();
    assert!(bank.join("consolidate.lock").is_file());
    drop(held);
}
