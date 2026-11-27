//! A real omp (`OMPDECK_TEST_OMP`, else skipped) under OmpDeck's session settings overlay
//! (`deck_core::omp::overlay`): with "Let sessions message each other" off the session stays out
//! of omp's same-folder agent mesh (`task.crossInstance`), and a running session follows each
//! rewrite of the overlay. Mesh membership = the leases of the `agents` unit omp's per-user daemon
//! reports for the project folder (`omp ps --json`).

mod support;

use std::time::{Duration, Instant};

use deck_core::omp::overlay;
use serde_json::Value;
use support::{Live, Reply, wait_for};

/// Sessions joined to the agent mesh of `live`'s project folder.
fn mesh_leases(live: &Live) -> u64 {
    let dir = std::fs::canonicalize(&live.proj).expect("project dir");
    // `omp ps` lists the current directory's scope unless told otherwise.
    let out = std::process::Command::new(live.omp.program())
        .args(["ps", "--json", "--dir"])
        .arg(&dir)
        .envs(live.env.iter().map(|(k, v)| (k, v)))
        .env_remove(overlay::CONFIG_FILES_ENV)
        .output()
        .expect("run omp ps");
    let Ok(ps) = serde_json::from_slice::<Value>(&out.stdout) else {
        return 0;
    };
    ps["reports"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|r| r["scope"]["kind"] == "project" && r["scope"]["dir"] == dir.to_str().unwrap())
        .flat_map(|r| r["units"].as_array().into_iter().flatten())
        .filter(|u| u["kind"] == "agents")
        .filter_map(|u| u["leases"].as_u64())
        .sum()
}

#[test]
fn the_messaging_toggle_keeps_a_running_session_in_or_out_of_omps_agent_mesh() {
    let dir = tempfile::tempdir().expect("temp dir");
    let file = dir.path().join(overlay::FILE_NAME);
    overlay::write(&file, false).expect("write the overlay");
    let files = overlay::config_files(&file).expect("PI_CONFIG_FILES");
    let Some(live) = Live::start_with_env(
        "xinst",
        |_| Reply::text("ok"),
        vec![(
            overlay::CONFIG_FILES_ENV.to_owned(),
            files.to_string_lossy().into_owned(),
        )],
    ) else {
        eprintln!("skipped: OMPDECK_TEST_OMP is not set");
        return;
    };

    // Off at launch: the session never joins (omp joins right after start when it may).
    let end = Instant::now() + Duration::from_secs(4);
    while Instant::now() < end {
        assert_eq!(
            mesh_leases(&live),
            0,
            "joined the mesh while messaging is off"
        );
        std::thread::sleep(Duration::from_millis(250));
    }

    // On: the running session joins without a restart.
    overlay::write(&file, true).expect("write the overlay");
    wait_for(
        "the session to join omp's agent mesh",
        support::TURN,
        || (mesh_leases(&live) >= 1).then_some(()),
    );

    // Off again: it leaves.
    overlay::write(&file, false).expect("write the overlay");
    wait_for(
        "the session to leave omp's agent mesh",
        support::TURN,
        || (mesh_leases(&live) == 0).then_some(()),
    );
}
