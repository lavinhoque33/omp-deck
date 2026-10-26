//! OmpDeck self-update: find the newest GitHub release, download and verify the asset that fits
//! the way OmpDeck was installed, and swap the installed binaries.
//!
//! Everything blocks (network, file and process work); callers run it on workers. The latest
//! version comes from the redirect of `<base>/releases/latest` (no API, so no rate limit). Running
//! `omp-deck-host` daemons survive a replacement: on Unix the new file is a new inode, on Windows
//! the running file is renamed aside first.
//!
//! Verification / sandbox use only: `OMP_DECK_UPDATE_KIND` overrides [`detect`] with `deb`, `rpm`,
//! `appimage:<file>`, `dir:<dir>` (a user dir, or a Windows dir on Windows) or `dev`; any other
//! value is `Unsupported`. With an overridden `deb`/`rpm`, [`Applied::executable`] is the `omp-deck`
//! next to the current executable instead of `/usr/bin/omp-deck`, so a sandboxed run never names a
//! system binary.

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

use crate::gdrive::transfer_http;
use crate::usage::run_bounded;

/// Where releases live; `OMP_DECK_UPDATE_BASE` replaces it (tests, sandboxes).
const DEFAULT_BASE: &str = "https://github.com/lavinhoque33/omp-deck";
const BASE_ENV: &str = "OMP_DECK_UPDATE_BASE";
const KIND_ENV: &str = "OMP_DECK_UPDATE_KIND";
const TAG_MARKER: &str = "/releases/tag/";
const INSTALL_TIMEOUT: Duration = Duration::from_secs(15 * 60);
/// Deadline of the whole release check.
const CHECK_TIMEOUT: Duration = Duration::from_secs(15);
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);
/// Largest `.sha256` file read.
const MAX_SHA_FILE: u64 = 4096;
/// Names of the installed files, in replacement order. The host goes first so a half-finished
/// update never leaves a new GUI talking to an old daemon launcher; the gateway sits between the
/// host it launches and the GUI that connects to it.
const UNIX_BINARIES: [&str; 3] = ["omp-deck-host", "omp-deck-gateway", "omp-deck"];
const WINDOWS_FILES: [&str; 5] = [
    "omp-deck-host.exe",
    "omp-deck-gateway.exe",
    "conpty.dll",
    "OpenConsole.exe",
    "omp-deck.exe",
];
/// Where deb/rpm packages install the GUI.
const PACKAGE_BIN: &str = "/usr/bin/omp-deck";

/// How this OmpDeck was installed.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum InstallKind {
    /// `install.sh` / tarball: a writable directory holding `omp-deck` and `omp-deck-host`.
    UserDir {
        dir: PathBuf,
    },
    AppImage {
        file: PathBuf,
    },
    Deb,
    Rpm,
    /// `%LOCALAPPDATA%\Programs\OmpDeck` (or any writable folder holding the zip's files).
    WindowsDir {
        dir: PathBuf,
    },
    /// A cargo `target/` directory: never updated.
    Dev,
    Unsupported {
        reason: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Release {
    /// `"0.9.0"` (tag without the leading `v`).
    pub version: String,
    /// `"v0.9.0"`.
    pub tag: String,
    /// `<base>/releases/tag/<tag>`.
    pub page_url: String,
}

/// The installed result, independent of which process applied it (GUI, gateway or supervisor).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Applied {
    /// The installed primary GUI executable (the AppImage file for AppImage installs). Callers
    /// decide what to restart and with which arguments; this never carries a process's argv.
    pub executable: PathBuf,
}

#[derive(Debug, thiserror::Error)]
pub enum UpdateError {
    #[error("network error: {0}")]
    Http(String),
    #[error("checksum verification failed: {0}")]
    Checksum(String),
    #[error("the release has no {0}")]
    NoAsset(String),
    #[error("{0}")]
    Unsupported(String),
    #[error("could not install the update automatically; run: {command}")]
    NeedsManual { command: String },
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("timed out")]
    Timeout,
    #[error("cancelled")]
    Cancelled,
}

// ---------------------------------------------------------------------------------------------
// Install kind

/// How the running OmpDeck is installed (`$APPIMAGE`, the location of `current_exe`, the owning
/// deb/rpm package for `/usr`, a writability probe of the install directory).
pub fn detect() -> InstallKind {
    if let Some(value) = kind_override() {
        return parse_kind_override(&value);
    }
    let appimage = std::env::var_os("APPIMAGE")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from);
    classify(appimage.as_deref(), std::env::current_exe().ok().as_deref())
}

fn kind_override() -> Option<String> {
    std::env::var(KIND_ENV)
        .ok()
        .filter(|v| !v.trim().is_empty())
}

/// Parses an `OMP_DECK_UPDATE_KIND` value.
fn parse_kind_override(value: &str) -> InstallKind {
    let value = value.trim();
    let unsupported = || InstallKind::Unsupported {
        reason: format!(
            "invalid {KIND_ENV} value `{value}` (expected deb, rpm, dev, appimage:<file> or dir:<dir>)"
        ),
    };
    match value.split_once(':') {
        None => match value {
            "deb" => InstallKind::Deb,
            "rpm" => InstallKind::Rpm,
            "dev" => InstallKind::Dev,
            _ => unsupported(),
        },
        Some(("appimage", file)) if !file.is_empty() => InstallKind::AppImage {
            file: PathBuf::from(file),
        },
        Some(("dir", dir)) if !dir.is_empty() => {
            let dir = PathBuf::from(dir);
            if cfg!(windows) {
                InstallKind::WindowsDir { dir }
            } else {
                InstallKind::UserDir { dir }
            }
        }
        Some(_) => unsupported(),
    }
}

fn classify(appimage: Option<&Path>, exe: Option<&Path>) -> InstallKind {
    classify_with(appimage, exe, &owning_package)
}

/// [`classify`] with the package-ownership probe injected.
fn classify_with(
    appimage: Option<&Path>,
    exe: Option<&Path>,
    owner: &dyn Fn(&Path) -> Option<InstallKind>,
) -> InstallKind {
    if let Some(file) = appimage.filter(|f| f.is_file()) {
        return match file.parent() {
            Some(dir) if dir_writable(dir) => InstallKind::AppImage {
                file: file.to_path_buf(),
            },
            _ => InstallKind::Unsupported {
                reason: format!("the folder of {} is not writable", file.display()),
            },
        };
    }
    let Some(exe) = exe else {
        return InstallKind::Unsupported {
            reason: "the location of the running executable is unknown".into(),
        };
    };
    if in_cargo_target(exe) {
        return InstallKind::Dev;
    }
    let Some(dir) = exe.parent() else {
        return InstallKind::Unsupported {
            reason: "the running executable has no folder".into(),
        };
    };
    // A package-owned file is updated through the package manager; anything else under `/usr`
    // (a tarball in `/usr/local/bin`) is classified by writability like any other folder.
    if cfg!(unix)
        && exe.starts_with("/usr")
        && let Some(kind) = owner(exe)
    {
        return kind;
    }
    if !dir_writable(dir) {
        return InstallKind::Unsupported {
            reason: format!("{} is not writable", dir.display()),
        };
    }
    let dir = dir.to_path_buf();
    if cfg!(windows) {
        InstallKind::WindowsDir { dir }
    } else {
        InstallKind::UserDir { dir }
    }
}

/// True when `exe` lives below a `target` directory whose parent holds a `Cargo.toml`.
fn in_cargo_target(exe: &Path) -> bool {
    exe.ancestors().skip(1).any(|a| {
        a.file_name() == Some(OsStr::new("target"))
            && a.parent().is_some_and(|p| p.join("Cargo.toml").is_file())
    })
}

fn dir_writable(dir: &Path) -> bool {
    tempfile::Builder::new()
        .prefix(".omp-deck-probe")
        .tempfile_in(dir)
        .is_ok()
}

/// `dpkg -S` / `rpm -qf` on a system path.
fn owning_package(exe: &Path) -> Option<InstallKind> {
    let owned = |program: &str, args: &[&OsStr]| -> bool {
        let Some(bin) = crate::platform::find_on_path(program) else {
            return false;
        };
        let mut cmd = std::process::Command::new(bin);
        cmd.args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        matches!(run_bounded(cmd, PROBE_TIMEOUT), Ok((Some(0), _, _)))
    };
    if owned("dpkg", &[OsStr::new("-S"), exe.as_os_str()]) {
        Some(InstallKind::Deb)
    } else if owned("rpm", &[OsStr::new("-qf"), exe.as_os_str()]) {
        Some(InstallKind::Rpm)
    } else {
        None
    }
}

// ---------------------------------------------------------------------------------------------
// Latest release

fn base_url() -> String {
    std::env::var(BASE_ENV)
        .ok()
        .map(|v| v.trim().trim_end_matches('/').to_owned())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| DEFAULT_BASE.to_owned())
}

fn map_http(e: ureq::Error) -> UpdateError {
    match e {
        ureq::Error::Timeout(_) => UpdateError::Timeout,
        other => UpdateError::Http(other.to_string()),
    }
}

fn map_read(e: std::io::Error) -> UpdateError {
    if e.kind() == std::io::ErrorKind::TimedOut {
        UpdateError::Timeout
    } else {
        UpdateError::Http(e.to_string())
    }
}

/// The newest published release, from the redirect of `<base>/releases/latest`.
pub fn latest() -> Result<Release, UpdateError> {
    latest_from(&base_url())
}

/// Agent for the release check: no redirects followed (the `Location` is the answer) and a
/// short overall deadline, unlike the stall-based one of downloads.
fn check_http() -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .max_redirects(0)
        .timeout_global(Some(CHECK_TIMEOUT))
        .user_agent(concat!("omp-deck/", env!("CARGO_PKG_VERSION")))
        .build()
        .into()
}

fn latest_from(base: &str) -> Result<Release, UpdateError> {
    let resp = check_http()
        .get(format!("{base}/releases/latest"))
        .call()
        .map_err(map_http)?;
    let status = resp.status().as_u16();
    if !(300..400).contains(&status) {
        return Err(UpdateError::Http(format!(
            "expected a redirect to the latest release, got HTTP {status}"
        )));
    }
    let location = resp
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| UpdateError::Http("the redirect has no Location".into()))?;
    let (_, rest) = location.split_once(TAG_MARKER).ok_or_else(|| {
        UpdateError::Http(format!(
            "no release published yet (redirected to {location})"
        ))
    })?;
    let tag = rest
        .split(['/', '?', '#'])
        .next()
        .filter(|t| !t.is_empty())
        .ok_or_else(|| UpdateError::Http("the redirect names no tag".into()))?;
    let version = tag.strip_prefix('v').unwrap_or(tag);
    if parse_version(version).is_none() {
        return Err(UpdateError::Http(format!(
            "the latest release tag `{tag}` is not a version"
        )));
    }
    Ok(Release {
        version: version.to_owned(),
        tag: tag.to_owned(),
        page_url: format!("{base}{TAG_MARKER}{tag}"),
    })
}

/// `(numeric parts, pre-release identifiers)` of `1.2.3-rc.1+build`.
fn parse_version(v: &str) -> Option<(Vec<u64>, Option<Vec<String>>)> {
    let v = v.trim().trim_start_matches('v');
    let v = v.split('+').next()?;
    let (core, pre) = match v.split_once('-') {
        Some((c, p)) => (c, Some(p)),
        None => (v, None),
    };
    let nums: Option<Vec<u64>> = core.split('.').map(|p| p.parse().ok()).collect();
    let nums = nums.filter(|n| !n.is_empty())?;
    let pre = pre.map(|p| p.split('.').map(str::to_owned).collect());
    Some((nums, pre))
}

/// Numeric semver comparison: `latest` is newer than `current`. A pre-release is older than its
/// release; unparsable versions are never newer.
pub fn is_newer(latest: &str, current: &str) -> bool {
    use std::cmp::Ordering::{Equal, Greater, Less};
    let (Some((ln, lp)), Some((cn, cp))) = (parse_version(latest), parse_version(current)) else {
        return false;
    };
    let width = ln.len().max(cn.len());
    let pad = |mut n: Vec<u64>| {
        n.resize(width, 0);
        n
    };
    match pad(ln).cmp(&pad(cn)) {
        Greater => true,
        Less => false,
        Equal => match (lp, cp) {
            (None, Some(_)) => true,
            (Some(l), Some(c)) => pre_cmp(&l, &c) == Greater,
            _ => false,
        },
    }
}

/// Semver pre-release precedence: numeric identifiers compare numerically and sort below
/// alphanumeric ones; a longer list wins a tie.
fn pre_cmp(a: &[String], b: &[String]) -> std::cmp::Ordering {
    use std::cmp::Ordering::{Equal, Greater, Less};
    for (x, y) in a.iter().zip(b) {
        let ord = match (x.parse::<u64>(), y.parse::<u64>()) {
            (Ok(x), Ok(y)) => x.cmp(&y),
            (Ok(_), Err(_)) => Less,
            (Err(_), Ok(_)) => Greater,
            (Err(_), Err(_)) => x.cmp(y),
        };
        if ord != Equal {
            return ord;
        }
    }
    a.len().cmp(&b.len())
}

// ---------------------------------------------------------------------------------------------
// Assets

/// Release asset for `kind` on this machine's architecture.
pub fn asset_for(kind: &InstallKind, version: &str) -> Option<String> {
    asset_for_arch(kind, version, std::env::consts::ARCH)
}

fn asset_for_arch(kind: &InstallKind, v: &str, arch: &str) -> Option<String> {
    let x64 = arch == "x86_64";
    match kind {
        InstallKind::UserDir { .. } if x64 => Some(format!("omp-deck-{v}-x86_64-linux.tar.gz")),
        InstallKind::AppImage { .. } if x64 => Some(format!("omp-deck-{v}-x86_64.AppImage")),
        InstallKind::Deb if x64 => Some(format!("omp-deck_{v}_amd64.deb")),
        InstallKind::Rpm if x64 => Some(format!("omp-deck-{v}-1.x86_64.rpm")),
        InstallKind::WindowsDir { .. } if x64 || arch == "aarch64" => {
            Some(format!("omp-deck-{v}-{arch}-windows.zip"))
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------------------------
// Download

fn asset_url(release: &Release, asset: &str) -> Result<String, UpdateError> {
    let (base, _) = release
        .page_url
        .split_once(TAG_MARKER)
        .ok_or_else(|| UpdateError::Http(format!("unusable release page {}", release.page_url)))?;
    Ok(format!("{base}/releases/download/{}/{asset}", release.tag))
}

fn first_sha256(text: &str) -> Option<String> {
    let token = text.split_whitespace().next()?;
    (token.len() == 64 && token.bytes().all(|b| b.is_ascii_hexdigit()))
        .then(|| token.to_ascii_lowercase())
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::with_capacity(64), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

fn status_error(status: u16, asset: &str) -> UpdateError {
    if status == 404 {
        UpdateError::NoAsset(format!("asset {asset}"))
    } else {
        UpdateError::Http(format!("HTTP {status} for {asset}"))
    }
}

/// Downloads `asset` of `release` into `dir` and verifies it against `<asset>.sha256` (published
/// next to it). The checksum is fetched first and a release without one is refused. A mismatch
/// deletes the file. `progress(done, total)`; `cancel` aborts and removes the partial file.
pub fn download(
    release: &Release,
    asset: &str,
    dir: &Path,
    progress: &dyn Fn(u64, Option<u64>),
    cancel: &AtomicBool,
) -> Result<PathBuf, UpdateError> {
    if asset.is_empty() || asset.contains(['/', '\\']) || asset == ".." || asset == "." {
        return Err(UpdateError::Unsupported(format!(
            "invalid asset name `{asset}`"
        )));
    }
    let url = asset_url(release, asset)?;
    let http = transfer_http(true);

    let mut sum_resp = http.get(format!("{url}.sha256")).call().map_err(map_http)?;
    let status = sum_resp.status().as_u16();
    if status == 404 {
        return Err(UpdateError::Checksum(format!(
            "the release publishes no checksum for {asset}"
        )));
    }
    if !(200..300).contains(&status) {
        return Err(status_error(status, &format!("{asset}.sha256")));
    }
    let mut sum_text = String::new();
    sum_resp
        .body_mut()
        .as_reader()
        .take(MAX_SHA_FILE)
        .read_to_string(&mut sum_text)
        .map_err(map_read)?;
    let expected = first_sha256(&sum_text).ok_or_else(|| {
        UpdateError::Checksum(format!("{asset}.sha256 does not hold a SHA-256 digest"))
    })?;

    let mut resp = http.get(&url).call().map_err(map_http)?;
    let status = resp.status().as_u16();
    if !(200..300).contains(&status) {
        return Err(status_error(status, asset));
    }
    let total = resp
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());

    std::fs::create_dir_all(dir)?;
    let part = dir.join(format!("{asset}.part"));
    let result = stream_to(&part, resp.body_mut().as_reader(), total, progress, cancel);
    let actual = match result {
        Ok(actual) => actual,
        Err(e) => {
            let _ = std::fs::remove_file(&part);
            return Err(e);
        }
    };
    if actual != expected {
        let _ = std::fs::remove_file(&part);
        return Err(UpdateError::Checksum(format!(
            "{asset} has SHA-256 {actual}, expected {expected}"
        )));
    }
    let dest = dir.join(asset);
    if let Err(e) = std::fs::rename(&part, &dest) {
        let _ = std::fs::remove_file(&part);
        return Err(e.into());
    }
    Ok(dest)
}

/// Copies `body` to `path` hashing as it goes; returns the hex SHA-256.
fn stream_to(
    path: &Path,
    mut body: impl Read,
    total: Option<u64>,
    progress: &dyn Fn(u64, Option<u64>),
    cancel: &AtomicBool,
) -> Result<String, UpdateError> {
    let mut out = File::create(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut done = 0u64;
    progress(0, total);
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(UpdateError::Cancelled);
        }
        let n = body.read(&mut buf).map_err(map_read)?;
        if n == 0 {
            break;
        }
        out.write_all(&buf[..n])?;
        hasher.update(&buf[..n]);
        done += n as u64;
        progress(done, total);
    }
    out.flush()?;
    Ok(hex(&hasher.finalize()))
}

// ---------------------------------------------------------------------------------------------
// Apply

/// Installs the downloaded asset. The caller removes the download directory afterwards.
pub fn apply(kind: &InstallKind, downloaded: &Path) -> Result<Applied, UpdateError> {
    let executable = match kind {
        InstallKind::UserDir { dir } => {
            apply_tarball(dir, downloaded)?;
            dir.join("omp-deck")
        }
        InstallKind::AppImage { file } => {
            install_executable(downloaded, file)?;
            file.clone()
        }
        InstallKind::WindowsDir { dir } => {
            apply_windows_zip(dir, downloaded, &timestamp(SystemTime::now()))?;
            dir.join("omp-deck.exe")
        }
        InstallKind::Deb | InstallKind::Rpm => {
            install_package(kind, downloaded)?;
            package_executable()
        }
        InstallKind::Dev => {
            return Err(UpdateError::Unsupported(
                "a development build is updated by rebuilding".into(),
            ));
        }
        InstallKind::Unsupported { reason } => {
            return Err(UpdateError::Unsupported(reason.clone()));
        }
    };
    Ok(Applied { executable })
}

/// `/usr/bin/omp-deck`, or the GUI beside the current executable when the kind was overridden.
fn package_executable() -> PathBuf {
    package_executable_for(kind_override().is_some(), std::env::current_exe().ok())
}

fn package_executable_for(overridden: bool, current: Option<PathBuf>) -> PathBuf {
    match current.as_deref().and_then(Path::parent) {
        Some(dir) if overridden => dir.join(if cfg!(windows) {
            "omp-deck.exe"
        } else {
            "omp-deck"
        }),
        _ => PathBuf::from(PACKAGE_BIN),
    }
}

fn invalid(msg: impl Into<String>) -> UpdateError {
    UpdateError::Io(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        msg.into(),
    ))
}

fn new_temp_in(dir: &Path) -> std::io::Result<tempfile::NamedTempFile> {
    tempfile::Builder::new()
        .prefix(".omp-deck-update-")
        .suffix(".new")
        .tempfile_in(dir)
}

#[cfg(unix)]
fn make_executable(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
}

#[cfg(not(unix))]
fn make_executable(_: &Path) -> std::io::Result<()> {
    Ok(())
}

/// Copies `src` next to `dest` (0755) and renames it over `dest`.
fn install_executable(src: &Path, dest: &Path) -> Result<(), UpdateError> {
    let dir = dest
        .parent()
        .ok_or_else(|| invalid(format!("{} has no folder", dest.display())))?;
    let mut tmp = new_temp_in(dir)?;
    std::io::copy(&mut File::open(src)?, tmp.as_file_mut())?;
    tmp.as_file_mut().sync_all()?;
    make_executable(tmp.path())?;
    tmp.persist(dest).map_err(|e| UpdateError::Io(e.error))?;
    Ok(())
}

/// Unpacks `omp-deck`, `omp-deck-gateway` and `omp-deck-host` from the release tarball into temp files in `dir`
/// (0755), then renames them over the installed files, host first.
fn apply_tarball(dir: &Path, tarball: &Path) -> Result<(), UpdateError> {
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(File::open(tarball)?));
    let mut staged: Vec<Option<tempfile::NamedTempFile>> =
        UNIX_BINARIES.iter().map(|_| None).collect();
    for entry in archive.entries()? {
        let mut entry = entry?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let path = entry.path()?.into_owned();
        // `<top-dir>/omp-deck` or a bare `omp-deck`; deeper copies are something else.
        if path.components().count() > 2 {
            continue;
        }
        let Some(name) = path.file_name().and_then(OsStr::to_str) else {
            continue;
        };
        let Some(slot) = UNIX_BINARIES.iter().position(|b| *b == name) else {
            continue;
        };
        if staged[slot].is_some() {
            continue;
        }
        let mut tmp = new_temp_in(dir)?;
        std::io::copy(&mut entry, tmp.as_file_mut())?;
        tmp.as_file_mut().sync_all()?;
        make_executable(tmp.path())?;
        staged[slot] = Some(tmp);
    }
    for (name, tmp) in UNIX_BINARIES.iter().zip(&staged) {
        if tmp.is_none() {
            return Err(invalid(format!("the archive has no {name}")));
        }
    }
    for (name, tmp) in UNIX_BINARIES.iter().zip(staged) {
        if let Some(tmp) = tmp {
            tmp.persist(dir.join(name))
                .map_err(|e| UpdateError::Io(e.error))?;
        }
    }
    Ok(())
}

/// Windows layout: for each file of the zip, rename the installed one to `<name>.old-<stamp>`
/// (a running executable can be renamed, not overwritten) and write the new one. Any failure
/// puts every renamed file back and removes what was written.
fn apply_windows_zip(dir: &Path, zip_path: &Path, stamp: &str) -> Result<(), UpdateError> {
    let mut archive =
        zip::ZipArchive::new(File::open(zip_path)?).map_err(|e| invalid(e.to_string()))?;
    // Entry index per wanted name (Compress-Archive may store `\` separators).
    let mut index: Vec<Option<usize>> = vec![None; WINDOWS_FILES.len()];
    for i in 0..archive.len() {
        let file = archive.by_index(i).map_err(|e| invalid(e.to_string()))?;
        if file.is_dir() {
            continue;
        }
        let Ok(name) = file.name() else { continue };
        let name = name.replace('\\', "/");
        let leaf = name.rsplit('/').next().unwrap_or_default();
        if let Some(slot) = WINDOWS_FILES.iter().position(|w| *w == leaf)
            && index[slot].is_none()
        {
            index[slot] = Some(i);
        }
    }
    for (name, idx) in WINDOWS_FILES.iter().zip(&index) {
        if idx.is_none() {
            return Err(invalid(format!("the zip has no {name}")));
        }
    }

    // (installed path, aside path) of every file renamed so far, and files written so far.
    let mut asides: Vec<(PathBuf, PathBuf)> = Vec::new();
    let mut written: Vec<PathBuf> = Vec::new();
    let mut run = || -> Result<(), UpdateError> {
        for (name, idx) in WINDOWS_FILES.iter().zip(&index) {
            let Some(idx) = *idx else { continue };
            let target = dir.join(name);
            if target.exists() {
                let aside = dir.join(format!("{name}.old-{stamp}"));
                std::fs::rename(&target, &aside)?;
                asides.push((target.clone(), aside));
            }
            let mut file = archive.by_index(idx).map_err(|e| invalid(e.to_string()))?;
            let mut out = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&target)?;
            written.push(target);
            std::io::copy(&mut file, &mut out)?;
            out.sync_all()?;
        }
        Ok(())
    };
    match run() {
        Ok(()) => Ok(()),
        Err(e) => {
            for path in written.iter().rev() {
                let _ = std::fs::remove_file(path);
            }
            for (target, aside) in asides.iter().rev() {
                if let Err(err) = std::fs::rename(aside, target) {
                    log::error!("rollback: cannot restore {}: {err}", target.display());
                }
            }
            Err(e)
        }
    }
}

/// `yyyyMMddHHmmss` (UTC).
fn timestamp(t: SystemTime) -> String {
    let secs = t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let (days, rem) = (secs / 86_400, secs % 86_400);
    // Howard Hinnant's civil-from-days.
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}{month:02}{day:02}{:02}{:02}{:02}",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// What installing a package needs: the privileged command and the same command for a terminal.
#[derive(Debug, PartialEq, Eq)]
struct PackagePlan {
    /// Program and arguments run through `pkexec`.
    command: Vec<OsString>,
    /// What the user can paste into a terminal instead.
    manual: String,
}

fn shell_quote(s: &str) -> String {
    let plain = !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"/._-+=:@%".contains(&b));
    if plain {
        s.to_owned()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

/// dnf/apt-get when present, else rpm/dpkg. `has` tells whether a program is on `PATH`.
fn plan_package_install(
    kind: &InstallKind,
    file: &Path,
    has: &dyn Fn(&str) -> bool,
) -> Result<PackagePlan, UpdateError> {
    let words: Vec<&str> = match kind {
        InstallKind::Deb if has("apt-get") => vec!["apt-get", "install", "-y"],
        InstallKind::Deb => vec!["dpkg", "-i"],
        InstallKind::Rpm if has("dnf") => vec!["dnf", "install", "-y"],
        InstallKind::Rpm => vec!["rpm", "-U"],
        _ => return Err(UpdateError::Unsupported("not a package install".into())),
    };
    let mut command: Vec<OsString> = words.iter().map(OsString::from).collect();
    command.push(file.as_os_str().to_owned());
    let manual = format!(
        "sudo {} {}",
        words.join(" "),
        shell_quote(&file.to_string_lossy())
    );
    Ok(PackagePlan { command, manual })
}

fn install_package(kind: &InstallKind, downloaded: &Path) -> Result<(), UpdateError> {
    let file = downloaded
        .canonicalize()
        .unwrap_or_else(|_| downloaded.to_path_buf());
    let plan = plan_package_install(kind, &file, &|p| crate::platform::find_on_path(p).is_some())?;
    let manual = || UpdateError::NeedsManual {
        command: plan.manual.clone(),
    };
    let Some(pkexec) = crate::platform::find_on_path("pkexec") else {
        return Err(manual());
    };
    let mut cmd = std::process::Command::new(pkexec);
    cmd.args(&plan.command)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let (code, stdout, stderr) = run_bounded(cmd, INSTALL_TIMEOUT).map_err(|e| match e {
        crate::usage::UsageError::Timeout(_) => UpdateError::Timeout,
        crate::usage::UsageError::Spawn(e) => UpdateError::Io(e),
        other => UpdateError::Http(other.to_string()),
    })?;
    match code {
        Some(0) => Ok(()),
        // pkexec: 126 = the authentication dialog was dismissed, 127 = not authorized.
        Some(126 | 127) | None => Err(manual()),
        Some(c) => {
            let text = if stderr.trim().is_empty() {
                &stdout
            } else {
                &stderr
            };
            let tail: Vec<&str> = text.lines().rev().take(6).collect();
            let tail: Vec<&str> = tail.into_iter().rev().collect();
            Err(UpdateError::Io(std::io::Error::other(format!(
                "package install failed (exit {c}): {}",
                tail.join("\n")
            ))))
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Leftovers

/// Windows: deletes the `<name>.old-<stamp>` files a previous update renamed aside next to the
/// running executable (best effort; a still-running old file stays until the next start).
/// No-op elsewhere.
pub fn sweep_leftovers() {
    #[cfg(windows)]
    {
        if let Some(dir) = std::env::current_exe()
            .ok()
            .and_then(|e| e.parent().map(Path::to_path_buf))
        {
            sweep_dir(&dir);
        }
    }
}

/// `true` for `<managed name>.old-<14 digits>`.
#[cfg(any(windows, test))]
fn is_leftover(name: &str) -> bool {
    let managed = |n: &str| WINDOWS_FILES.contains(&n) || UNIX_BINARIES.contains(&n);
    name.rsplit_once(".old-").is_some_and(|(base, stamp)| {
        managed(base) && stamp.len() == 14 && stamp.bytes().all(|b| b.is_ascii_digit())
    })
}

#[cfg(any(windows, test))]
fn sweep_dir(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if name.to_str().is_some_and(is_leftover) && std::fs::remove_file(entry.path()).is_err() {
            log::debug!("update leftover {} is still in use", entry.path().display());
        }
    }
}

#[cfg(test)]
mod tests;
