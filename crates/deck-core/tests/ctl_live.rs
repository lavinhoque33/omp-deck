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
    #[cfg(target_os = "linux")]
    if let Some(real) = dirs::home_dir()
        .map(|h| h.join(".omp/natives"))
        .filter(|n| n.exists())
    {
        std::os::unix::fs::symlink(real, home.join(".omp/natives")).unwrap();
    }
    let mut env = vec![
        kv("HOME", &home),
        kv("USERPROFILE", &home),
        kv("PI_CODING_AGENT_DIR", &agent),
        kv("PI_CONFIG_DIR", home.join(".omp")),
    ];
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

    // The real client finds the extension's endpoint once omp has started.
    let client = CtlClient::spawn(dir, || {});
    let end = Instant::now() + Duration::from_secs(30);
    while client.latest().is_none() && Instant::now() < end {
        std::thread::sleep(Duration::from_millis(100));
    }
    let snap = client.latest().unwrap_or_else(|| {
        let log = host
            .namespace()
            .log_path(name)
            .and_then(|p| fs::read_to_string(p).ok());
        panic!("no snapshot within 30 s; host log: {log:?}");
    });
    assert!(client.connected());
    assert!(snap.pid > 0);
    let file = snap
        .session_file
        .as_deref()
        .expect("snapshot names a session file");
    assert_eq!(
        file.file_name(),
        session.file_name(),
        "snapshot names {file:?}, expected {session:?}"
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
