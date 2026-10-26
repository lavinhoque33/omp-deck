use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::AtomicBool;

use super::*;

// ---- a tiny HTTP server ---------------------------------------------------------------------

struct Route {
    status: u16,
    location: Option<String>,
    body: Vec<u8>,
}

fn ok(body: &[u8]) -> Route {
    Route {
        status: 200,
        location: None,
        body: body.to_vec(),
    }
}

fn redirect(to: &str) -> Route {
    Route {
        status: 302,
        location: Some(to.to_owned()),
        body: Vec::new(),
    }
}

/// Serves `routes` (path → response; anything else is a 404) on a free local port; returns the
/// base URL. The server threads end with the test process.
fn serve(routes: Vec<(String, Route)>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let routes: std::sync::Arc<HashMap<String, Route>> =
        std::sync::Arc::new(routes.into_iter().collect());
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let routes = routes.clone();
            std::thread::spawn(move || handle(stream, &routes));
        }
    });
    format!("http://127.0.0.1:{port}/lavinhoque33/omp-deck")
}

fn handle(mut stream: std::net::TcpStream, routes: &HashMap<String, Route>) {
    let mut req = Vec::new();
    let mut buf = [0u8; 1024];
    while !req.windows(4).any(|w| w == b"\r\n\r\n") {
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => return,
            Ok(n) => req.extend_from_slice(&buf[..n]),
        }
    }
    let line = String::from_utf8_lossy(&req);
    let path = line.split_whitespace().nth(1).unwrap_or("/").to_owned();
    let fallback = Route {
        status: 404,
        location: None,
        body: b"not found".to_vec(),
    };
    let route = routes.get(&path).unwrap_or(&fallback);
    let mut head = format!(
        "HTTP/1.1 {} X\r\nContent-Length: {}\r\nConnection: close\r\n",
        route.status,
        route.body.len()
    );
    if let Some(l) = &route.location {
        head.push_str(&format!("Location: {l}\r\n"));
    }
    head.push_str("\r\n");
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(&route.body);
}

fn sha(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

const REPO: &str = "/lavinhoque33/omp-deck";

/// A server with `latest` redirecting to v0.9.0 and `asset` (+ checksum) under its download path.
fn release_server(asset: &str, body: &[u8], digest: &str) -> (String, Release) {
    let base = serve(vec![
        (
            format!("{REPO}/releases/latest"),
            redirect(&format!("{REPO}/releases/tag/v0.9.0")),
        ),
        (format!("{REPO}/releases/download/v0.9.0/{asset}"), ok(body)),
        (
            format!("{REPO}/releases/download/v0.9.0/{asset}.sha256"),
            ok(format!("{digest}  {asset}\n").as_bytes()),
        ),
    ]);
    let release = latest_from(&base).expect("latest");
    (base, release)
}

fn no_progress(_: u64, _: Option<u64>) {}

// ---- versions -------------------------------------------------------------------------------

#[test]
fn is_newer_compares_numerically_and_ranks_prereleases_below_their_release() {
    let cases = [
        ("0.9.0", "0.8.0", true),
        ("0.8.0", "0.8.0", false),
        ("0.8.0", "0.9.0", false),
        ("0.10.0", "0.9.0", true),
        ("1.0.0", "0.99.99", true),
        ("0.8.1", "0.8.0", true),
        ("0.8", "0.8.0", false),
        ("0.8.0.1", "0.8.0", true),
        ("v0.9.0", "0.8.0", true),
        ("0.9.0+build5", "0.9.0", false),
        // A pre-release is never newer than its own release …
        ("0.9.0-rc1", "0.9.0", false),
        ("0.9.0", "0.9.0-rc1", true),
        // … and orders among its siblings.
        ("0.9.0-rc2", "0.9.0-rc1", true),
        ("0.9.0-rc.10", "0.9.0-rc.9", true),
        ("0.9.0-rc1", "0.9.0-rc1", false),
        ("0.9.0-1", "0.9.0-alpha", false),
        ("0.9.0-alpha.1", "0.9.0-alpha", true),
        ("0.9.0-rc1", "0.8.0", true),
        // Garbage is never an update.
        ("nightly", "0.8.0", false),
        ("0.9.0", "dev", false),
        ("", "", false),
        ("1.x", "1.0", false),
    ];
    for (latest, current, want) in cases {
        assert_eq!(
            is_newer(latest, current),
            want,
            "is_newer({latest:?}, {current:?})"
        );
    }
}

#[test]
fn assets_follow_the_release_naming_per_kind_and_arch() {
    let dir = PathBuf::from("/x");
    let user = InstallKind::UserDir { dir: dir.clone() };
    let appimage = InstallKind::AppImage { file: dir.clone() };
    let win = InstallKind::WindowsDir { dir };
    let v = "0.9.0";
    assert_eq!(
        asset_for_arch(&user, v, "x86_64").as_deref(),
        Some("omp-deck-0.9.0-x86_64-linux.tar.gz")
    );
    assert_eq!(
        asset_for_arch(&appimage, v, "x86_64").as_deref(),
        Some("omp-deck-0.9.0-x86_64.AppImage")
    );
    assert_eq!(
        asset_for_arch(&InstallKind::Deb, v, "x86_64").as_deref(),
        Some("omp-deck_0.9.0_amd64.deb")
    );
    assert_eq!(
        asset_for_arch(&InstallKind::Rpm, v, "x86_64").as_deref(),
        Some("omp-deck-0.9.0-1.x86_64.rpm")
    );
    assert_eq!(
        asset_for_arch(&win, v, "x86_64").as_deref(),
        Some("omp-deck-0.9.0-x86_64-windows.zip")
    );
    assert_eq!(
        asset_for_arch(&win, v, "aarch64").as_deref(),
        Some("omp-deck-0.9.0-aarch64-windows.zip")
    );
    // No Linux build exists for other architectures, nor a Windows one for 32-bit.
    for kind in [&user, &appimage, &InstallKind::Deb, &InstallKind::Rpm] {
        assert_eq!(asset_for_arch(kind, v, "aarch64"), None);
    }
    assert_eq!(asset_for_arch(&win, v, "x86"), None);
    assert_eq!(asset_for_arch(&InstallKind::Dev, v, "x86_64"), None);
    let unsupported = InstallKind::Unsupported { reason: "r".into() };
    assert_eq!(asset_for_arch(&unsupported, v, "x86_64"), None);
}

// ---- detect ---------------------------------------------------------------------------------

#[test]
fn a_cargo_target_executable_is_a_dev_build() {
    let proj = tempfile::tempdir().expect("tempdir");
    std::fs::write(proj.path().join("Cargo.toml"), "[workspace]\n").expect("write");
    let exe = proj
        .path()
        .join("target/x86_64-unknown-linux-gnu/release/omp-deck");
    std::fs::create_dir_all(exe.parent().expect("parent")).expect("mkdir");
    std::fs::write(&exe, "bin").expect("write");
    assert_eq!(classify(None, Some(&exe)), InstallKind::Dev);

    // A `target` folder that is not cargo's is just a folder.
    let plain = tempfile::tempdir().expect("tempdir");
    let exe = plain.path().join("target/omp-deck");
    std::fs::create_dir_all(exe.parent().expect("parent")).expect("mkdir");
    assert!(matches!(
        classify(None, Some(&exe)),
        InstallKind::UserDir { .. } | InstallKind::WindowsDir { .. }
    ));
}

#[test]
fn a_writable_install_folder_is_updatable_and_a_missing_one_is_not() {
    let dir = tempfile::tempdir().expect("tempdir");
    let exe = dir.path().join("omp-deck");
    std::fs::write(&exe, "bin").expect("write");
    let kind = classify(None, Some(&exe));
    if cfg!(windows) {
        assert_eq!(
            kind,
            InstallKind::WindowsDir {
                dir: dir.path().to_path_buf()
            }
        );
    } else {
        assert_eq!(
            kind,
            InstallKind::UserDir {
                dir: dir.path().to_path_buf()
            }
        );
    }
    assert!(
        dir.path().read_dir().expect("ls").count() == 1,
        "the probe cleans up"
    );

    let gone = dir.path().join("nope/omp-deck");
    assert!(matches!(
        classify(None, Some(&gone)),
        InstallKind::Unsupported { .. }
    ));
    assert!(matches!(
        classify(None, None),
        InstallKind::Unsupported { .. }
    ));
}

#[test]
fn an_existing_appimage_wins_over_the_executable_location() {
    let dir = tempfile::tempdir().expect("tempdir");
    let image = dir.path().join("OmpDeck.AppImage");
    std::fs::write(&image, "img").expect("write");
    let exe = PathBuf::from("/tmp/.mount_abc/usr/bin/omp-deck");
    assert_eq!(
        classify(Some(&image), Some(&exe)),
        InstallKind::AppImage {
            file: image.clone()
        }
    );
    // A stale $APPIMAGE (file gone) falls through to the executable.
    let stale = dir.path().join("gone.AppImage");
    assert!(matches!(
        classify(Some(&stale), Some(&exe)),
        InstallKind::Unsupported { .. }
    ));
}

// ---- latest ---------------------------------------------------------------------------------

#[test]
fn latest_reads_the_tag_from_a_relative_or_absolute_redirect() {
    let (base, release) = release_server("a", b"x", &sha(b"x"));
    assert_eq!(release.version, "0.9.0");
    assert_eq!(release.tag, "v0.9.0");
    assert_eq!(release.page_url, format!("{base}/releases/tag/v0.9.0"));

    let absolute = serve(vec![(
        format!("{REPO}/releases/latest"),
        redirect("https://github.com/lavinhoque33/omp-deck/releases/tag/v1.2.3-rc1?x=1"),
    )]);
    let r = latest_from(&absolute).expect("latest");
    assert_eq!(
        (r.version.as_str(), r.tag.as_str()),
        ("1.2.3-rc1", "v1.2.3-rc1")
    );
}

#[test]
fn latest_rejects_answers_that_are_not_a_release_redirect() {
    let cases = [
        // No release yet: GitHub redirects to the releases list.
        redirect(&format!("{REPO}/releases")),
        redirect(&format!("{REPO}/releases/tag/nightly")),
        redirect(&format!("{REPO}/releases/tag/")),
        ok(b"<html>a page</html>"),
        Route {
            status: 404,
            location: None,
            body: Vec::new(),
        },
    ];
    for route in cases {
        let base = serve(vec![(format!("{REPO}/releases/latest"), route)]);
        let err = latest_from(&base).expect_err("must fail");
        assert!(matches!(err, UpdateError::Http(_)), "{err}");
    }
}

#[test]
fn latest_reports_an_unreachable_server_as_http() {
    let port = {
        let l = TcpListener::bind("127.0.0.1:0").expect("bind");
        l.local_addr().expect("addr").port()
    };
    let err = latest_from(&format!("http://127.0.0.1:{port}/x")).expect_err("refused");
    assert!(matches!(err, UpdateError::Http(_)), "{err}");
}

// ---- download -------------------------------------------------------------------------------

#[test]
fn download_streams_verifies_and_reports_progress() {
    let body: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
    let asset = "omp-deck-0.9.0-x86_64.AppImage";
    let (_, release) = release_server(asset, &body, &sha(&body));
    let out = tempfile::tempdir().expect("tempdir");
    let seen = std::sync::Mutex::new(Vec::new());
    let path = download(
        &release,
        asset,
        out.path(),
        &|done, total| seen.lock().expect("lock").push((done, total)),
        &AtomicBool::new(false),
    )
    .expect("download");
    assert_eq!(path, out.path().join(asset));
    assert_eq!(std::fs::read(&path).expect("read"), body);
    let seen = seen.into_inner().expect("lock");
    let total = Some(body.len() as u64);
    assert!(seen.iter().all(|(_, t)| *t == total));
    assert!(seen.windows(2).all(|w| w[0].0 <= w[1].0));
    assert_eq!(seen.last().map(|p| p.0), Some(body.len() as u64));
    let names: Vec<_> = out.path().read_dir().expect("ls").flatten().collect();
    assert_eq!(names.len(), 1, "no .part file is left behind");
}

#[test]
fn a_checksum_mismatch_deletes_the_download() {
    let asset = "omp-deck_0.9.0_amd64.deb";
    let (_, release) = release_server(asset, b"tampered", &sha(b"original"));
    let out = tempfile::tempdir().expect("tempdir");
    let err = download(
        &release,
        asset,
        out.path(),
        &no_progress,
        &AtomicBool::new(false),
    )
    .expect_err("mismatch");
    assert!(matches!(err, UpdateError::Checksum(_)), "{err}");
    assert_eq!(out.path().read_dir().expect("ls").count(), 0);
}

#[test]
fn a_release_without_a_checksum_file_is_refused() {
    let asset = "a.tar.gz";
    let base = serve(vec![
        (
            format!("{REPO}/releases/latest"),
            redirect(&format!("{REPO}/releases/tag/v0.9.0")),
        ),
        (
            format!("{REPO}/releases/download/v0.9.0/{asset}"),
            ok(b"data"),
        ),
    ]);
    let release = latest_from(&base).expect("latest");
    let out = tempfile::tempdir().expect("tempdir");
    let err = download(
        &release,
        asset,
        out.path(),
        &no_progress,
        &AtomicBool::new(false),
    )
    .expect_err("no checksum");
    assert!(matches!(err, UpdateError::Checksum(_)), "{err}");
    assert_eq!(out.path().read_dir().expect("ls").count(), 0);
}

#[test]
fn a_garbled_checksum_file_is_refused() {
    let asset = "a.tar.gz";
    let base = serve(vec![
        (
            format!("{REPO}/releases/latest"),
            redirect(&format!("{REPO}/releases/tag/v0.9.0")),
        ),
        (
            format!("{REPO}/releases/download/v0.9.0/{asset}"),
            ok(b"data"),
        ),
        (
            format!("{REPO}/releases/download/v0.9.0/{asset}.sha256"),
            ok(b"<html>oops</html>"),
        ),
    ]);
    let release = latest_from(&base).expect("latest");
    let out = tempfile::tempdir().expect("tempdir");
    let err = download(
        &release,
        asset,
        out.path(),
        &no_progress,
        &AtomicBool::new(false),
    )
    .expect_err("garbled");
    assert!(matches!(err, UpdateError::Checksum(_)), "{err}");
}

#[test]
fn a_missing_asset_is_no_asset() {
    let out = tempfile::tempdir().expect("tempdir");
    // The checksum is fetched first: publish it so the asset itself is what is missing.
    let base = serve(vec![
        (
            format!("{REPO}/releases/latest"),
            redirect(&format!("{REPO}/releases/tag/v0.9.0")),
        ),
        (
            format!("{REPO}/releases/download/v0.9.0/gone.sha256"),
            ok(format!("{}  gone\n", sha(b"x")).as_bytes()),
        ),
    ]);
    let release2 = latest_from(&base).expect("latest");
    let err = download(
        &release2,
        "gone",
        out.path(),
        &no_progress,
        &AtomicBool::new(false),
    )
    .expect_err("missing");
    assert!(matches!(err, UpdateError::NoAsset(_)), "{err}");
}

#[test]
fn cancelling_stops_the_download_and_removes_the_partial_file() {
    let body = vec![7u8; 4 * 1024 * 1024];
    let asset = "big.AppImage";
    let (_, release) = release_server(asset, &body, &sha(&body));
    let out = tempfile::tempdir().expect("tempdir");
    let cancel = AtomicBool::new(false);
    let err = download(
        &release,
        asset,
        out.path(),
        &|done, _| {
            if done > 0 {
                cancel.store(true, Ordering::Relaxed);
            }
        },
        &cancel,
    )
    .expect_err("cancelled");
    assert!(matches!(err, UpdateError::Cancelled), "{err}");
    assert_eq!(out.path().read_dir().expect("ls").count(), 0);
}

#[test]
fn asset_names_cannot_escape_the_download_folder() {
    let (_, release) = release_server("a", b"x", &sha(b"x"));
    let out = tempfile::tempdir().expect("tempdir");
    for bad in ["", "..", "../x", "a/b", "a\\b"] {
        let err = download(
            &release,
            bad,
            out.path(),
            &no_progress,
            &AtomicBool::new(false),
        )
        .expect_err("invalid");
        assert!(matches!(err, UpdateError::Unsupported(_)), "{bad:?}: {err}");
    }
}

// ---- apply ----------------------------------------------------------------------------------

fn make_tar_gz(path: &Path, entries: &[(&str, &[u8])]) {
    let gz = flate2::write::GzEncoder::new(
        File::create(path).expect("create"),
        flate2::Compression::fast(),
    );
    let mut tar = tar::Builder::new(gz);
    for (name, data) in entries {
        let mut header = tar::Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        tar.append_data(&mut header, name, *data).expect("append");
    }
    tar.into_inner()
        .expect("finish tar")
        .finish()
        .expect("finish gz");
}

fn names(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = dir
        .read_dir()
        .expect("ls")
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    v.sort();
    v
}

#[cfg(unix)]
fn mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).expect("meta").permissions().mode() & 0o777
}

#[test]
fn user_dir_update_replaces_gui_gateway_and_host_with_executable_files() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("omp-deck"), "old gui").expect("write");
    std::fs::write(dir.path().join("omp-deck-host"), "old host").expect("write");
    std::fs::write(dir.path().join("omp-deck-gateway"), "old gateway").expect("write");
    std::fs::write(dir.path().join("keep.txt"), "mine").expect("write");
    let dl = tempfile::tempdir().expect("tempdir");
    let tarball = dl.path().join("omp-deck-0.9.0-x86_64-linux.tar.gz");
    make_tar_gz(
        &tarball,
        &[
            ("omp-deck-0.9.0-x86_64-linux/omp-deck", b"new gui"),
            ("omp-deck-0.9.0-x86_64-linux/omp-deck-host", b"new host"),
            (
                "omp-deck-0.9.0-x86_64-linux/omp-deck-gateway",
                b"new gateway",
            ),
            (
                "omp-deck-0.9.0-x86_64-linux/omp-deck.desktop",
                b"[Desktop Entry]",
            ),
            (
                "omp-deck-0.9.0-x86_64-linux/icons/omp-deck",
                b"not a binary",
            ),
        ],
    );
    let kind = InstallKind::UserDir {
        dir: dir.path().to_path_buf(),
    };
    let applied = apply(&kind, &tarball).expect("apply");

    assert_eq!(
        std::fs::read(dir.path().join("omp-deck")).expect("gui"),
        b"new gui"
    );
    assert_eq!(
        std::fs::read(dir.path().join("omp-deck-host")).expect("host"),
        b"new host"
    );
    assert_eq!(
        std::fs::read(dir.path().join("omp-deck-gateway")).expect("gateway"),
        b"new gateway"
    );
    assert_eq!(
        std::fs::read(dir.path().join("keep.txt")).expect("keep"),
        b"mine"
    );
    #[cfg(unix)]
    {
        assert_eq!(mode(&dir.path().join("omp-deck")), 0o755);
        assert_eq!(mode(&dir.path().join("omp-deck-host")), 0o755);
        assert_eq!(mode(&dir.path().join("omp-deck-gateway")), 0o755);
    }
    assert_eq!(
        names(dir.path()),
        ["keep.txt", "omp-deck", "omp-deck-gateway", "omp-deck-host"],
        "no temp files left"
    );
    assert_eq!(applied.executable, dir.path().join("omp-deck"));
}

#[test]
fn an_incomplete_tarball_changes_nothing() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("omp-deck"), "old gui").expect("write");
    std::fs::write(dir.path().join("omp-deck-host"), "old host").expect("write");
    let dl = tempfile::tempdir().expect("tempdir");
    let tarball = dl.path().join("t.tar.gz");
    make_tar_gz(&tarball, &[("x/omp-deck", b"new gui")]);
    let kind = InstallKind::UserDir {
        dir: dir.path().to_path_buf(),
    };
    let err = apply(&kind, &tarball).expect_err("missing host");
    assert!(err.to_string().contains("omp-deck-host"), "{err}");
    assert_eq!(
        std::fs::read(dir.path().join("omp-deck")).expect("gui"),
        b"old gui"
    );
    assert_eq!(
        std::fs::read(dir.path().join("omp-deck-host")).expect("host"),
        b"old host"
    );
    assert_eq!(names(dir.path()), ["omp-deck", "omp-deck-host"]);

    let not_gzip = dl.path().join("bad.tar.gz");
    std::fs::write(&not_gzip, "this is not an archive").expect("write");
    assert!(apply(&kind, &not_gzip).is_err());
    assert_eq!(names(dir.path()), ["omp-deck", "omp-deck-host"]);
}

#[test]
fn appimage_update_replaces_the_file_in_place() {
    let dir = tempfile::tempdir().expect("tempdir");
    let image = dir.path().join("OmpDeck.AppImage");
    std::fs::write(&image, "old image").expect("write");
    let dl = tempfile::tempdir().expect("tempdir");
    let new = dl.path().join("omp-deck-0.9.0-x86_64.AppImage");
    std::fs::write(&new, "new image").expect("write");
    let applied = apply(
        &InstallKind::AppImage {
            file: image.clone(),
        },
        &new,
    )
    .expect("apply");
    assert_eq!(std::fs::read(&image).expect("read"), b"new image");
    #[cfg(unix)]
    assert_eq!(mode(&image), 0o755);
    assert_eq!(names(dir.path()), ["OmpDeck.AppImage"]);
    assert_eq!(applied.executable, image);
}

#[test]
fn dev_and_unsupported_installs_refuse_to_apply() {
    let file = tempfile::NamedTempFile::new().expect("tmp");
    assert!(matches!(
        apply(&InstallKind::Dev, file.path()),
        Err(UpdateError::Unsupported(_))
    ));
    let kind = InstallKind::Unsupported {
        reason: "read-only".into(),
    };
    let err = apply(&kind, file.path()).expect_err("unsupported");
    assert_eq!(err.to_string(), "read-only");
}

fn make_zip(path: &Path, entries: &[(&str, &[u8])]) {
    let mut zip = zip::ZipWriter::new(File::create(path).expect("create"));
    for (name, data) in entries {
        zip.start_file(*name, zip::write::SimpleFileOptions::default())
            .expect("start");
        zip.write_all(data).expect("write");
    }
    zip.finish().expect("finish");
}

const STAMP: &str = "20260102030405";

fn windows_install(dir: &Path) {
    for n in WINDOWS_FILES {
        std::fs::write(dir.join(n), format!("old {n}")).expect("write");
    }
}

fn full_zip(path: &Path) {
    make_zip(
        path,
        &[
            ("README.md", b"docs"),
            ("omp-deck.exe", b"new gui"),
            // Compress-Archive on Windows PowerShell 5 stores backslash separators.
            ("stage\\omp-deck-host.exe", b"new host"),
            ("omp-deck-gateway.exe", b"new gateway"),
            ("conpty.dll", b"new conpty"),
            ("OpenConsole.exe", b"new console"),
        ],
    );
}

#[test]
fn windows_update_renames_files_aside_then_writes_the_new_ones() {
    let dir = tempfile::tempdir().expect("tempdir");
    windows_install(dir.path());
    let dl = tempfile::tempdir().expect("tempdir");
    let zip = dl.path().join("w.zip");
    full_zip(&zip);
    apply_windows_zip(dir.path(), &zip, STAMP).expect("apply");
    for (n, new) in [
        ("omp-deck.exe", "new gui"),
        ("omp-deck-host.exe", "new host"),
        ("omp-deck-gateway.exe", "new gateway"),
        ("conpty.dll", "new conpty"),
        ("OpenConsole.exe", "new console"),
    ] {
        assert_eq!(
            std::fs::read_to_string(dir.path().join(n)).expect("new"),
            new
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join(format!("{n}.old-{STAMP}"))).expect("aside"),
            format!("old {n}")
        );
    }
    assert!(
        !dir.path().join("README.md").exists(),
        "only the managed files are installed"
    );
    sweep_dir(dir.path());
    assert_eq!(
        names(dir.path()),
        [
            "OpenConsole.exe",
            "conpty.dll",
            "omp-deck-gateway.exe",
            "omp-deck-host.exe",
            "omp-deck.exe"
        ],
        "the sweep removes exactly the aside files"
    );
}

#[test]
fn windows_update_works_on_a_fresh_folder() {
    let dir = tempfile::tempdir().expect("tempdir");
    let dl = tempfile::tempdir().expect("tempdir");
    let zip = dl.path().join("w.zip");
    full_zip(&zip);
    apply_windows_zip(dir.path(), &zip, STAMP).expect("apply");
    assert_eq!(names(dir.path()).len(), 5);
}

#[test]
fn windows_update_rolls_everything_back_when_a_later_file_fails() {
    let dir = tempfile::tempdir().expect("tempdir");
    windows_install(dir.path());
    // The aside name of the last file (omp-deck.exe) is taken by a directory: its rename fails
    // after the first four files were already swapped.
    std::fs::create_dir(dir.path().join(format!("omp-deck.exe.old-{STAMP}"))).expect("mkdir");
    let dl = tempfile::tempdir().expect("tempdir");
    let zip = dl.path().join("w.zip");
    full_zip(&zip);
    apply_windows_zip(dir.path(), &zip, STAMP).expect_err("must fail");
    for n in WINDOWS_FILES {
        assert_eq!(
            std::fs::read_to_string(dir.path().join(n)).expect("restored"),
            format!("old {n}"),
            "{n} is back"
        );
    }
    assert_eq!(
        names(dir.path()),
        [
            "OpenConsole.exe",
            "conpty.dll",
            "omp-deck-gateway.exe",
            "omp-deck-host.exe",
            "omp-deck.exe",
            "omp-deck.exe.old-20260102030405"
        ],
        "no stray aside or new files"
    );
}

#[test]
fn a_zip_missing_a_file_is_rejected_before_anything_is_touched() {
    let dir = tempfile::tempdir().expect("tempdir");
    windows_install(dir.path());
    let dl = tempfile::tempdir().expect("tempdir");
    let zip = dl.path().join("w.zip");
    make_zip(
        &zip,
        &[
            ("omp-deck.exe", b"x"),
            ("omp-deck-host.exe", b"x"),
            ("omp-deck-gateway.exe", b"x"),
        ],
    );
    let err = apply_windows_zip(dir.path(), &zip, STAMP).expect_err("incomplete");
    assert!(err.to_string().contains("conpty.dll"), "{err}");
    assert_eq!(names(dir.path()).len(), 5);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("omp-deck.exe")).expect("gui"),
        "old omp-deck.exe"
    );
}

#[test]
fn timestamps_are_utc_yyyymmddhhmmss() {
    let at = |s: u64| timestamp(UNIX_EPOCH + Duration::from_secs(s));
    assert_eq!(at(0), "19700101000000");
    assert_eq!(at(1_700_000_000), "20231114221320");
    assert_eq!(at(1_709_164_799), "20240228235959");
    assert_eq!(at(1_709_164_800), "20240229000000", "leap day");
    assert_eq!(at(4_102_444_799), "20991231235959");
}

#[test]
fn only_aside_files_of_managed_names_are_leftovers() {
    assert!(is_leftover("omp-deck.exe.old-20260102030405"));
    assert!(is_leftover("conpty.dll.old-20260102030405"));
    assert!(is_leftover("omp-deck-host.old-20260102030405"));
    assert!(is_leftover("omp-deck-gateway.exe.old-20260102030405"));
    assert!(!is_leftover("omp-deck.exe"));
    assert!(!is_leftover("omp-deck.exe.old-2026"));
    assert!(!is_leftover("omp-deck.exe.old-2026010203040x"));
    assert!(!is_leftover("notes.txt.old-20260102030405"));
}

#[test]
fn package_installs_prefer_the_package_manager_and_quote_for_the_terminal() {
    let file = Path::new("/tmp/up date/omp-deck_0.9.0_amd64.deb");
    let both = |p: &str| matches!(p, "apt-get" | "dnf");
    let none = |_: &str| false;

    let plan = plan_package_install(&InstallKind::Deb, file, &both).expect("plan");
    assert_eq!(
        plan.command,
        [
            "apt-get",
            "install",
            "-y",
            "/tmp/up date/omp-deck_0.9.0_amd64.deb"
        ]
        .map(OsString::from)
    );
    assert_eq!(
        plan.manual,
        "sudo apt-get install -y '/tmp/up date/omp-deck_0.9.0_amd64.deb'"
    );
    let plan = plan_package_install(&InstallKind::Deb, file, &none).expect("plan");
    assert_eq!(
        plan.manual,
        "sudo dpkg -i '/tmp/up date/omp-deck_0.9.0_amd64.deb'"
    );

    let rpm = Path::new("/tmp/omp-deck-0.9.0-1.x86_64.rpm");
    let plan = plan_package_install(&InstallKind::Rpm, rpm, &both).expect("plan");
    assert_eq!(
        plan.manual,
        "sudo dnf install -y /tmp/omp-deck-0.9.0-1.x86_64.rpm"
    );
    let plan = plan_package_install(&InstallKind::Rpm, rpm, &none).expect("plan");
    assert_eq!(plan.manual, "sudo rpm -U /tmp/omp-deck-0.9.0-1.x86_64.rpm");
    assert_eq!(plan.command[0], OsString::from("rpm"));

    assert!(plan_package_install(&InstallKind::Dev, rpm, &both).is_err());
    assert_eq!(shell_quote("it's"), "'it'\\''s'");
}

#[test]
fn the_checksum_file_may_be_sha256sum_or_bare_output() {
    let digest = "a".repeat(64);
    assert_eq!(
        first_sha256(&format!("{digest}  file.tar.gz\n")).as_deref(),
        Some(digest.as_str())
    );
    assert_eq!(
        first_sha256(&digest.to_uppercase()).as_deref(),
        Some(digest.as_str())
    );
    assert_eq!(first_sha256("abc  file"), None);
    assert_eq!(first_sha256(&format!("{}g", "a".repeat(63))), None);
    assert_eq!(first_sha256(""), None);
}

#[cfg(unix)]
#[test]
fn only_package_owned_usr_files_use_the_package_manager() {
    let exe = Path::new("/usr/bin/omp-deck");
    let owned = classify_with(None, Some(exe), &|_| Some(InstallKind::Deb));
    assert_eq!(owned, InstallKind::Deb);

    // Unowned: classified by writability of its folder, never blanket-Unsupported.
    let unowned = classify_with(None, Some(Path::new("/usr/local/bin/omp-deck")), &|_| None);
    if dir_writable(Path::new("/usr/local/bin")) {
        assert_eq!(
            unowned,
            InstallKind::UserDir {
                dir: "/usr/local/bin".into()
            }
        );
    } else {
        assert!(
            matches!(&unowned, InstallKind::Unsupported { reason } if reason.contains("not writable")),
            "{unowned:?}"
        );
    }

    // Outside /usr the probe is never consulted.
    let dir = tempfile::tempdir().expect("tempdir");
    let kind = classify_with(None, Some(&dir.path().join("omp-deck")), &|_| {
        panic!("package probe must not run outside /usr")
    });
    assert_eq!(
        kind,
        InstallKind::UserDir {
            dir: dir.path().to_path_buf()
        }
    );
}

#[test]
fn install_kind_override_values_parse_strictly() {
    assert_eq!(parse_kind_override("deb"), InstallKind::Deb);
    assert_eq!(parse_kind_override(" rpm "), InstallKind::Rpm);
    assert_eq!(parse_kind_override("dev"), InstallKind::Dev);
    assert_eq!(
        parse_kind_override("appimage:/tmp/x y.AppImage"),
        InstallKind::AppImage {
            file: "/tmp/x y.AppImage".into()
        }
    );
    let dir = parse_kind_override("dir:/tmp/sandbox/bin");
    assert!(matches!(
        dir,
        InstallKind::UserDir { .. } | InstallKind::WindowsDir { .. }
    ));
    for bad in ["flatpak", "appimage:", "dir:", "deb:x", "snap:/x", "DEB"] {
        let kind = parse_kind_override(bad);
        assert!(
            matches!(&kind, InstallKind::Unsupported { reason } if reason.contains(&format!("`{bad}`"))),
            "{bad}: {kind:?}"
        );
    }
}

#[test]
fn overridden_package_kinds_name_the_gui_beside_the_running_executable() {
    // The running executable may be the gateway: the GUI is its sibling, never itself.
    let cur = Some(Path::new("/sandbox").join("omp-deck-gateway"));
    let gui = Path::new("/sandbox").join(if cfg!(windows) {
        "omp-deck.exe"
    } else {
        "omp-deck"
    });
    assert_eq!(package_executable_for(true, cur.clone()), gui);
    assert_eq!(
        package_executable_for(false, cur),
        PathBuf::from(PACKAGE_BIN)
    );
    assert_eq!(
        package_executable_for(true, None),
        PathBuf::from(PACKAGE_BIN)
    );
}

#[test]
fn latest_gives_up_on_a_server_that_never_answers() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    // Accept and hold the connections open without a word.
    let held =
        std::thread::spawn(move || listener.incoming().flatten().take(2).collect::<Vec<_>>());
    let started = std::time::Instant::now();
    let err = latest_from(&format!("http://127.0.0.1:{port}/x")).expect_err("silent server");
    assert!(matches!(err, UpdateError::Timeout), "{err}");
    assert!(
        started.elapsed() < Duration::from_secs(25),
        "{:?}",
        started.elapsed()
    );
    drop(held);
}
