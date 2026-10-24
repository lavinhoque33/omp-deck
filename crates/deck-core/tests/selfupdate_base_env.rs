//! `OMP_DECK_UPDATE_BASE` redirects the whole update flow to a local server. This is the only
//! test of the file (it owns the process environment).

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::AtomicBool;

use deck_core::selfupdate::{InstallKind, asset_for, download, is_newer, latest};
use sha2::{Digest, Sha256};

#[test]
fn update_flow_runs_against_the_base_override() {
    let body: Vec<u8> = (0..200_000u32).map(|i| (i % 13) as u8).collect();
    let digest: String = Sha256::digest(&body)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let version = "99.1.2";
    let asset = asset_for(&InstallKind::Deb, version);
    // Other architectures have no deb: the flow is x86_64-only, so name the asset directly.
    let asset = asset.unwrap_or_else(|| format!("omp-deck_{version}_amd64.deb"));

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let (asset_t, body_t) = (asset.clone(), body.clone());
    std::thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let (asset, body, digest) = (asset_t.clone(), body_t.clone(), digest.clone());
            std::thread::spawn(move || {
                let mut req = Vec::new();
                let mut buf = [0u8; 1024];
                while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                    match stream.read(&mut buf) {
                        Ok(0) | Err(_) => return,
                        Ok(n) => req.extend_from_slice(&buf[..n]),
                    }
                }
                let text = String::from_utf8_lossy(&req).into_owned();
                let path = text.split_whitespace().nth(1).unwrap_or("/");
                let (status, extra, payload): (u16, String, Vec<u8>) = if path
                    == "/mirror/releases/latest"
                {
                    (
                        302,
                        format!("Location: /mirror/releases/tag/v{version}\r\n"),
                        vec![],
                    )
                } else if path == format!("/mirror/releases/download/v{version}/{asset}") {
                    (200, String::new(), body)
                } else if path == format!("/mirror/releases/download/v{version}/{asset}.sha256") {
                    (
                        200,
                        String::new(),
                        format!("{digest}  {asset}\n").into_bytes(),
                    )
                } else {
                    (404, String::new(), vec![])
                };
                let head = format!(
                    "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n{extra}\r\n",
                    payload.len()
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(&payload);
            });
        }
    });

    // SAFETY: this test binary holds a single test; the only other thread (the listener above)
    // does not read the environment.
    unsafe {
        std::env::set_var(
            "OMP_DECK_UPDATE_BASE",
            format!("http://127.0.0.1:{port}/mirror/"),
        )
    };

    let release = latest().expect("latest");
    assert_eq!(release.version, version);
    assert_eq!(release.tag, format!("v{version}"));
    assert_eq!(
        release.page_url,
        format!("http://127.0.0.1:{port}/mirror/releases/tag/v{version}")
    );
    assert!(is_newer(&release.version, env!("CARGO_PKG_VERSION")));

    let dir = tempfile::tempdir().expect("tempdir");
    let path = download(
        &release,
        &asset,
        dir.path(),
        &|_, _| {},
        &AtomicBool::new(false),
    )
    .expect("download");
    assert_eq!(std::fs::read(path).expect("read"), body);
}
