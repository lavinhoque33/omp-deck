//! Live control against the REAL omp: the extension is installed the way the app does, omp runs
//! `--resume <session>` inside the OmpDeck host, and the real ctl client (socket on Unix, token
//! pipe on Windows) must receive hello + a snapshot naming that session.
//!
//! Runs only when `OMPDECK_TEST_OMP=<path to omp>` is set, and needs the `omp-deck-host` binary
//! next to the test executable's profile dir (`cargo build -p deck-host`); otherwise it returns
//! early with a note. Everything lives in a temp sandbox: HOME, `PI_*` and `XDG_*` point there.
//! No prompt is sent, so no model is called.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use deck_core::ctl::{
    CtlClient, ctl_dir, ctl_env, install_extension, prepare_ctl_dir, prune_ctl_dirs, with_ctl,
};
use deck_core::mux::{HostMux, Launch, Mux};

const USER: &str = r#"{"type":"message","id":"m1","parentId":null,"timestamp":"2026-01-01T00:00:01.000Z","message":{"role":"user","content":"hi","timestamp":1}}"#;

/// Removes the host session (and so omp) even when an assertion fails.
struct Cleanup {
    host: HostMux,
    session: String,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = self.host.namespace().kill(&self.session);
        // Leaves of this test namespace under the ctl root (no-op on Windows).
        prune_ctl_dirs(self.host.namespace().as_str(), &Default::default());
    }
}

fn host_exe() -> Option<PathBuf> {
    let profile_dir = std::env::current_exe()
        .ok()?
        .parent()?
        .parent()?
        .to_path_buf();
    let exe = profile_dir.join(format!("omp-deck-host{}", std::env::consts::EXE_SUFFIX));
    exe.is_file().then_some(exe)
}

/// A VT replay as plain text: escape sequences dropped, one line per screen row.
fn plain_text(vt: &[u8]) -> String {
    let text = String::from_utf8_lossy(vt);
    let mut out = String::new();
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => match chars.next() {
                // CSI: parameters, then a final byte in 0x40..=0x7e.
                Some('[') => {
                    for n in chars.by_ref() {
                        if ('@'..='~').contains(&n) {
                            break;
                        }
                    }
                }
                // OSC / APC: up to BEL or ST.
                Some(']' | '_') => {
                    while let Some(n) = chars.next() {
                        if n == '\x07' || (n == '\x1b' && chars.next() == Some('\\')) {
                            break;
                        }
                    }
                }
                _ => {}
            },
            '\r' => {}
            c => out.push(c),
        }
    }
    out
}

fn kv(k: &str, v: impl AsRef<Path>) -> (String, String) {
    (k.to_owned(), v.as_ref().to_string_lossy().into_owned())
}

#[test]
fn live_omp_serves_a_snapshot_of_its_session_through_the_host() {
    let Some(omp) = std::env::var_os("OMPDECK_TEST_OMP").map(PathBuf::from) else {
        eprintln!("skipping: OMPDECK_TEST_OMP is not set");
        return;
    };
    let Some(host_exe) = host_exe() else {
        eprintln!(
            "skipping: omp-deck-host not built next to the test profile (cargo build -p deck-host)"
        );
        return;
    };
    // SAFETY: this file holds a single test, so nothing else reads or writes the environment.
    unsafe { std::env::set_var("OMP_DECK_HOST_EXE", &host_exe) };

    // Sandbox.
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let agent = home.join(".omp/agent");
    let project = tmp.path().join("project");
    let data = tmp.path().join("data");
    for d in [&agent, &project, &data, &home.join(".config")] {
        fs::create_dir_all(d).unwrap();
    }
    let mut env = vec![
        kv("HOME", &home),
        kv("USERPROFILE", &home),
        kv("PI_CODING_AGENT_DIR", &agent),
        // Relative to HOME (omp joins it onto the home dir; an absolute path would be nested).
        ("PI_CONFIG_DIR".to_owned(), ".omp".to_owned()),
    ];
    // omp's extracted native addons: reuse the real cache (read-only use) instead of extracting
    // them again into the sandbox.
    if let Some(natives) = dirs::home_dir()
        .map(|h| h.join(".omp").join("natives"))
        .filter(|n| n.is_dir())
    {
        env.push(kv("PI_NATIVES_DIR", natives));
    }
    if cfg!(unix) {
        env.push(kv("XDG_CONFIG_HOME", home.join(".config")));
        env.push(kv("XDG_DATA_HOME", home.join(".local/share")));
        env.push(kv("XDG_CACHE_HOME", home.join(".cache")));
        env.push(kv("XDG_STATE_HOME", home.join(".local/state")));
    }

    // One-message session whose header names the project.
    let session_dir = agent.join("sessions/--live--");
    fs::create_dir_all(&session_dir).unwrap();
    let session = session_dir.join("2026-01-01T00-00-00-000Z_live0001.jsonl");
    let header = format!(
        r#"{{"type":"session","version":3,"id":"live0001","timestamp":"2026-01-01T00:00:00.000Z","cwd":{}}}"#,
        serde_json::to_string(&project.display().to_string()).unwrap()
    );
    fs::write(&session, format!("{header}\n{USER}\n")).unwrap();

    // The app's own steps: install the extension, prepare the endpoint, launch through the host.
    let ns_name = format!("omp-deck-test-ctl-{}", std::process::id());
    let name = "omp-live0001";
    let ext = install_extension(&data).unwrap();
    let dir = ctl_dir(&ns_name, name);
    prepare_ctl_dir(&dir).unwrap();
    let mut launch_env = env;
    launch_env.extend(
        ctl_env(&dir)
            .unwrap()
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v.to_string_lossy().into_owned())),
    );
    let argv = with_ctl(
        &[
            omp.display().to_string(),
            "--resume".into(),
            session.display().to_string(),
        ],
        &ext,
    );
    let host = HostMux::new(&ns_name).unwrap();
    let mux = Mux::Host(host.clone());
    let Launch::Host(spec) = mux.launch_with_env(name, &project, &argv, &launch_env) else {
        panic!("the host backend launches through the host");
    };
    let _cleanup = Cleanup {
        host: host.clone(),
        session: name.to_owned(),
    };
    deck_host::start(host.namespace(), &spec, 120, 40).unwrap();
    assert!(mux.has_session(name));

    // The real client finds the extension's endpoint once omp has started. While waiting, keep
    // the last screen of the session: omp's own error shows there if it exits early (the host
    // lingers 10 s after an unseen exit).
    let client = CtlClient::spawn(dir.clone(), || {});
    let start = Instant::now();
    let mut screen = String::new();
    let mut last_peek = Instant::now();
    while client.latest().is_none() && start.elapsed() < Duration::from_secs(30) {
        std::thread::sleep(Duration::from_millis(100));
        if last_peek.elapsed() >= Duration::from_millis(500) {
            last_peek = Instant::now();
            if let Ok((_, vt)) = deck_host::peek(host.namespace(), name, 200) {
                screen = plain_text(&vt);
            }
        }
    }
    let diagnostics = || {
        let log = host
            .namespace()
            .log_path(name)
            .and_then(|p| fs::read_to_string(p).ok());
        let final_screen = deck_host::peek(host.namespace(), name, 200)
            .map(|(_, vt)| plain_text(&vt))
            .unwrap_or(screen.clone());
        format!(
            "launch: {} {:?}\nenv set: {:?}\nenv removed: {:?}\nsession file: {}\nextension: {}\nctl endpoint: {}\nhost log: {log:?}\nomp screen:\n{final_screen}",
            spec.program.display(),
            spec.args,
            spec.env_set,
            spec.env_remove,
            session.display(),
            ext.display(),
            dir.display(),
        )
    };
    let Some(snap) = client.latest() else {
        panic!("no snapshot within 30 s\n{}", diagnostics());
    };
    assert!(client.connected());
    assert!(snap.pid > 0);
    let file = snap
        .session_file
        .as_deref()
        .unwrap_or_else(|| panic!("snapshot names no session file\n{}", diagnostics()));
    assert_eq!(
        file.file_name(),
        session.file_name(),
        "snapshot names {file:?}, expected {session:?}\n{}",
        diagnostics()
    );
    // A fresh resumed session with no subagents.
    assert!(snap.agents.is_empty(), "{:?}", snap.agents);

    mux.kill(name).unwrap();
    let end = Instant::now() + Duration::from_secs(10);
    while mux.has_session(name) && Instant::now() < end {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        !mux.has_session(name),
        "the host session is gone after kill"
    );
}
