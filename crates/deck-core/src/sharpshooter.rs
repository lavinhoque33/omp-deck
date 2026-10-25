//! omp's "sharpshooter" project memory: where a project's bank lives and how to take omp's
//! consolidation lock for it. Facts and sources: `docs/notes/sharpshooter-memory.md`.
//!
//! A bank is `<memories dir>/sharpshooter/<bank id>/` holding `architecture.md`, `product.md` and
//! `style.md`. The bank id is derived from the project's path alone (omp's
//! `sharpshooterBankId`): `<sanitized folder name>-<Bun.hash(path) in base 36>`, so the same
//! project at the same path has the same bank on every machine.

use std::ffi::OsString;
use std::fmt;
use std::io;
use std::path::{Component, MAIN_SEPARATOR, Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use xxhash_rust::xxh64::xxh64;

use crate::platform;
use crate::sessions::SessionsRoot;

#[cfg(test)]
mod tests;

/// The memory files of a bank, in omp's injection order.
pub const MEMORY_FILES: [&str; 3] = ["architecture.md", "product.md", "style.md"];

/// Retry interval while a lock is held (omp's own is 100 ms).
const LOCK_RETRY: Duration = Duration::from_millis(100);
/// Bank names longer than this are shortened (omp's `limitBankName`).
const MAX_NAME: usize = 64;

// ---------------------------------------------------------------- Wyhash

/// Zig `std.hash.Wyhash` secrets.
const SECRET: [u64; 4] = [
    0xa076_1d64_78bd_642f,
    0xe703_7ed1_a0b4_28db,
    0x8ebc_6af0_9c88_c6e3,
    0x5899_65cc_7537_4cc3,
];

fn mix(a: u64, b: u64) -> u64 {
    let p = u128::from(a) * u128::from(b);
    (p as u64) ^ ((p >> 64) as u64)
}

/// `N` little-endian bytes of `b` at `at`, zero-extended. Callers keep `at + N <= b.len()`.
fn read<const N: usize>(b: &[u8], at: usize) -> u64 {
    let mut word = [0u8; 8];
    word[..N].copy_from_slice(&b[at..at + N]);
    u64::from_le_bytes(word)
}

/// Zig std.hash.Wyhash, seed 0 (= `Bun.hash`). Pinned by vectors generated with bun.
pub fn wyhash(bytes: &[u8]) -> u64 {
    let len = bytes.len();
    let mut seed = mix(SECRET[0], SECRET[1]);
    let (mut a, mut b);
    if len <= 16 {
        if len >= 4 {
            let q = (len >> 3) << 2;
            a = (read::<4>(bytes, 0) << 32) | read::<4>(bytes, q);
            b = (read::<4>(bytes, len - 4) << 32) | read::<4>(bytes, len - 4 - q);
        } else if len > 0 {
            a = (u64::from(bytes[0]) << 16)
                | (u64::from(bytes[len >> 1]) << 8)
                | u64::from(bytes[len - 1]);
            b = 0;
        } else {
            a = 0;
            b = 0;
        }
    } else {
        let (mut at, mut left) = (0, len);
        if left > 48 {
            let (mut s1, mut s2) = (seed, seed);
            while left > 48 {
                seed = mix(
                    read::<8>(bytes, at) ^ SECRET[1],
                    read::<8>(bytes, at + 8) ^ seed,
                );
                s1 = mix(
                    read::<8>(bytes, at + 16) ^ SECRET[2],
                    read::<8>(bytes, at + 24) ^ s1,
                );
                s2 = mix(
                    read::<8>(bytes, at + 32) ^ SECRET[3],
                    read::<8>(bytes, at + 40) ^ s2,
                );
                at += 48;
                left -= 48;
            }
            seed ^= s1 ^ s2;
        }
        while left > 16 {
            seed = mix(
                read::<8>(bytes, at) ^ SECRET[1],
                read::<8>(bytes, at + 8) ^ seed,
            );
            at += 16;
            left -= 16;
        }
        a = read::<8>(bytes, at + left - 16);
        b = read::<8>(bytes, at + left - 8);
    }
    a ^= SECRET[1];
    b ^= seed;
    let p = u128::from(a) * u128::from(b);
    mix(
        (p as u64) ^ SECRET[0] ^ len as u64,
        ((p >> 64) as u64) ^ SECRET[1],
    )
}

fn base36(mut n: u64) -> String {
    const DIGITS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    if n == 0 {
        return "0".to_owned();
    }
    let mut out = Vec::new();
    while n > 0 {
        out.push(DIGITS[(n % 36) as usize]);
        n /= 36;
    }
    out.reverse();
    // Only ASCII digits and lowercase letters were pushed.
    String::from_utf8_lossy(&out).into_owned()
}

// ---------------------------------------------------------------- bank id

/// `\\?\C:\x` → `C:\x`, `\\?\UNC\server\share\x` → `\\server\share\x`; anything else unchanged.
/// omp sees `process.cwd()` strings, which never carry the verbatim prefix.
fn strip_verbatim(path: &str) -> Option<String> {
    let rest = path.strip_prefix(r"\\?\")?;
    if let Some(unc) = rest
        .get(..4)
        .filter(|p| p.eq_ignore_ascii_case(r"UNC\"))
        .map(|_| &rest[4..])
    {
        return Some(format!(r"\\{unc}"));
    }
    let b = rest.as_bytes();
    (b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':').then(|| rest.to_owned())
}

/// Node's `path.resolve` on `path`, lexically: made absolute against the current directory,
/// `.` dropped, `..` applied, no trailing separator (except on a root). Non-UTF-8 → lossy.
/// On Windows this is `path.win32.resolve`: verbatim prefixes dropped, drive-relative inputs
/// (`C:foo`) completed the way `GetFullPathNameW` does, a UNC share root ends in `\`.
fn resolve(path: &Path) -> String {
    let stripped = if cfg!(windows) {
        strip_verbatim(&path.to_string_lossy())
    } else {
        None
    };
    let path = stripped.as_deref().map_or(path, Path::new);
    let joined = if path.is_absolute() {
        path.to_owned()
    } else if cfg!(windows) {
        std::path::absolute(path).unwrap_or_else(|_| path.to_owned())
    } else if path.is_relative() {
        std::env::current_dir().map_or_else(|_| path.to_owned(), |cwd| cwd.join(path))
    } else {
        path.to_owned()
    };
    let (mut prefix, mut rooted, mut parts) = (String::new(), false, Vec::<String>::new());
    for c in joined.components() {
        match c {
            Component::Prefix(p) => {
                // A bare UNC share (`\\server\share`) is already the root of itself.
                rooted |= matches!(p.kind(), std::path::Prefix::UNC(..));
                prefix = p.as_os_str().to_string_lossy().into_owned();
            }
            Component::RootDir => rooted = true,
            Component::CurDir => {}
            Component::ParentDir => {
                parts.pop();
            }
            Component::Normal(s) => parts.push(s.to_string_lossy().into_owned()),
        }
    }
    let sep = MAIN_SEPARATOR.to_string();
    let mut out = prefix;
    if rooted {
        out.push(MAIN_SEPARATOR);
    }
    out.push_str(&parts.join(&sep));
    out
}

/// omp's `limitBankName`: names over 64 characters become `<prefix>-<hash of the name>`.
/// The name is ASCII by the time it gets here (sanitized, or digits of a hash).
fn limit(name: &str) -> String {
    if name.len() <= MAX_NAME {
        return name.to_owned();
    }
    let hash = base36(wyhash(name.as_bytes()));
    let prefix_len = (MAX_NAME - 1).saturating_sub(hash.len()).max(1);
    let prefix = name[..prefix_len].trim_end_matches('-');
    let prefix = if prefix.is_empty() { "bank" } else { prefix };
    format!("{prefix}-{hash}")
}

/// omp's `sanitizeBankName`: runs of characters outside `[A-Za-z0-9_-]` become one `-`, leading
/// and trailing `-` are stripped; nothing left → `None`. (omp trims whitespace first; that
/// whitespace would only have become `-` at the ends and been stripped, so it is not repeated.)
fn sanitize(name: &str) -> Option<String> {
    let mut out = String::with_capacity(name.len());
    let mut in_run = false;
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
            out.push(ch);
            in_run = false;
        } else if !in_run {
            out.push('-');
            in_run = true;
        }
    }
    let out = out.trim_matches('-');
    (!out.is_empty()).then(|| limit(out))
}

/// omp's `sharpshooterBankId(cwd)`: lexical path.resolve of `project`, sanitize/limit,
/// `<name>-<base36 hash>`. Hashes the platform path string (UTF-8; non-UTF-8 → lossy).
/// `project` is absolute (a relative one is taken against the current directory, like omp).
pub fn bank_id(project: &Path) -> String {
    let root = resolve(project);
    let name = basename(&root)
        .and_then(sanitize)
        .unwrap_or_else(|| "default".to_owned());
    limit(&format!("{name}-{}", base36(wyhash(root.as_bytes()))))
}

/// Node's `path.basename` of a [`resolve`] result. A Windows UNC share root
/// (`\\server\share\`) is named after the share, a drive root has no name.
fn basename(root: &str) -> Option<&str> {
    let last = root.rsplit(MAIN_SEPARATOR).next().unwrap_or("");
    if last.is_empty() && MAIN_SEPARATOR == '\\' && root.starts_with(r"\\") {
        return root.trim_end_matches('\\').rsplit('\\').next();
    }
    Some(last)
}

// ---------------------------------------------------------------- locations

/// What the memories directory depends on besides the agent directory; injectable for tests.
struct Env {
    home: PathBuf,
    /// `PI_CONFIG_DIR` when set and non-empty.
    config_dir: Option<OsString>,
    /// `XDG_STATE_HOME` when set and non-empty.
    xdg_state_home: Option<OsString>,
    /// omp only follows XDG on Linux and macOS.
    xdg_platform: bool,
}

impl Env {
    /// Read the same way as [`SessionsRoot::resolve`].
    fn current() -> Env {
        let non_empty = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty());
        Env {
            home: platform::home_or_root(),
            config_dir: non_empty("PI_CONFIG_DIR"),
            xdg_state_home: non_empty("XDG_STATE_HOME"),
            xdg_platform: cfg!(any(target_os = "linux", target_os = "macos")),
        }
    }
}

/// omp's `getMemoriesDir` + `sharpshooter`. State is under `$XDG_STATE_HOME/omp` instead of the
/// agent directory when the agent directory is the default `<home>/<PI_CONFIG_DIR or .omp>/agent`
/// (so not an explicit `PI_CODING_AGENT_DIR` elsewhere) and that XDG directory exists.
fn root_dir_in(agent_dir: &Path, env: &Env) -> PathBuf {
    let default_agent = env
        .home
        .join(env.config_dir.as_deref().unwrap_or_else(|| ".omp".as_ref()))
        .join("agent");
    // omp compares `path.resolve(PI_CODING_AGENT_DIR)` with the default: both sides lexically.
    if env.xdg_platform
        && resolve(agent_dir) == resolve(&default_agent)
        && let Some(state) = &env.xdg_state_home
    {
        let app = PathBuf::from(state).join("omp");
        if app.exists() {
            return app.join("memories").join("sharpshooter");
        }
    }
    agent_dir.join("memories").join("sharpshooter")
}

/// `<memories dir>/sharpshooter` for the agent dir that holds `root`.
pub fn root_dir(root: &SessionsRoot) -> PathBuf {
    root_dir_in(&root.agent_dir(), &Env::current())
}

/// The bank of `project`: `root_dir / bank_id`.
pub fn bank_dir(root: &SessionsRoot, project: &Path) -> PathBuf {
    root_dir(root).join(bank_id(project))
}

// ---------------------------------------------------------------- consolidation lock

/// omp's lock name for the lock path string (`file_lock/mod.rs` `memory_lock_name`).
fn lock_name(path: &str) -> String {
    const HIGH_SEED: u64 = 0x4f4d_502d_4c4f_434b;
    const LOW_SEED: u64 = 0x5049_2d46_494c_454c;
    let high = xxh64(path.as_bytes(), HIGH_SEED);
    let low = xxh64(path.as_bytes(), LOW_SEED);
    format!("omp-file-lock-{high:016x}{low:016x}")
}

/// omp's consolidation lock for a bank: it locks `path.resolve(<bank>/consolidate) + ".lock"`.
fn lock_path(bank: &Path) -> String {
    format!("{}.lock", resolve(&bank.join("consolidate")))
}

/// omp's consolidation lock for a bank (`<bank>/consolidate.lock`; Linux abstract socket, Windows
/// Global mutex, other Unix flock, which also creates the bank folder). Retries every 100 ms up
/// to `wait`; `Ok(None)` = still held by omp. Released on drop.
pub fn lock_bank(bank: &Path, wait: Duration) -> io::Result<Option<BankLock>> {
    let path = lock_path(bank);
    let name = lock_name(&path);
    let path = PathBuf::from(path);
    let deadline = Instant::now() + wait;
    loop {
        if let Some(held) = platform::try_named_lock(&name, &path)? {
            return Ok(Some(BankLock { _held: held }));
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Ok(None);
        }
        thread::sleep(left.min(LOCK_RETRY));
    }
}

/// A held consolidation lock; released on drop.
pub struct BankLock {
    _held: platform::NamedLock,
}

impl fmt::Debug for BankLock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("BankLock")
    }
}
