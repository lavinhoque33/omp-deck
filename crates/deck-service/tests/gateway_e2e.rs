//! The gateway as a real process with real omp, no GUI: concurrent startup gives one owner, a
//! session is launched and controlled through the private IPC alone, a lost owner is replaced and
//! re-adopts the live backend session, and a stopped session is only that session.
//!
//! Everything runs in a private HOME/XDG tree under a unique `omp-deck-test-*` namespace (backend
//! sockets included); the real omp store and the real `omp-deck` tmux server are never touched.
//! Skipped (with a message) when omp is not installed.

use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::Arc;
use std::time::{Duration, Instant};

use deck_service::controller::{Command as ControlCommand, Delivery, Runtime};
use deck_service::gateway::Profile;
use deck_service::gateway::client::{GatewayClient, GatewayOptions, Link};
use deck_service::gateway::protocol::{
    Answer, ControllerInfo, Done, LaunchSpec, Mutation, Outcome, Query, Role, SessionRole, Topic,
    TopicValue,
};
use deck_service::session_actions::Action;

const GATEWAY: &str = env!("CARGO_BIN_EXE_omp-deck-gateway");

struct Sandbox {
    root: PathBuf,
    ns: String,
    profile: Profile,
}

impl Sandbox {
    fn new() -> Sandbox {
        let ns = format!("omp-deck-test-gwe2e-{}", std::process::id());
        let root = std::env::temp_dir().join(&ns);
        let _ = std::fs::remove_dir_all(&root);
        for d in ["home", "config", "data", "state", "run", "proj"] {
            std::fs::create_dir_all(root.join(d)).expect("sandbox dir");
        }
        let envs: [(&str, PathBuf); 5] = [
            ("HOME", root.join("home")),
            ("XDG_CONFIG_HOME", root.join("config")),
            ("XDG_DATA_HOME", root.join("data")),
            ("XDG_STATE_HOME", root.join("state")),
            ("XDG_RUNTIME_DIR", root.join("run")),
        ];
        // SAFETY: the only test of this binary; set before any thread exists.
        unsafe {
            for (k, v) in envs {
                std::env::set_var(k, v);
            }
            std::env::remove_var("PI_CODING_AGENT_DIR");
            std::env::set_var("OMP_DECK_HOST_NS", &ns);
            std::env::set_var("OMP_DECK_TMUX_SOCKET", &ns);
            std::env::set_var("PI_NOTIFICATIONS", "off");
        }
        // No first-run wizard, no collab: omp starts straight into its editor.
        let agent = root.join("home/.omp/agent");
        std::fs::create_dir_all(&agent).expect("agent dir");
        std::fs::write(
            agent.join("config.yml"),
            "startup:\n  setupWizard: false\ncollab:\n  autoStart: off\n",
        )
        .expect("omp config");
        let profile = Profile {
            host_namespace: ns.clone(),
            settings: root.join("config/omp-deck/state.json"),
            sessions_root: root.join("home/.omp/agent/sessions"),
        };
        Sandbox { root, ns, profile }
    }

    fn gateway<S: AsRef<std::ffi::OsStr>>(&self, args: &[S]) -> Output {
        Command::new(GATEWAY)
            .args(args)
            .output()
            .expect("run omp-deck-gateway")
    }

    fn owner_pid(&self) -> Option<u32> {
        let out = self.gateway(&["status", "--json"]);
        if !out.status.success() {
            return None;
        }
        let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
        v["status"]["pid"].as_u64().map(|p| p as u32)
    }

    fn client(&self) -> GatewayClient {
        let mut opts = GatewayOptions::new(self.profile.clone(), Role::Gui, Arc::new(|| {}));
        opts.gateway_exe = Some(PathBuf::from(GATEWAY));
        GatewayClient::start(opts)
    }

    fn proj(&self) -> PathBuf {
        self.root.join("proj")
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        // Our own test server only (never the default server, never the real `omp-deck` one).
        let _ = Command::new("tmux")
            .args(["-L", &self.ns, "kill-server"])
            .output();
        if let Some(pid) = self.owner_pid() {
            let _ = Command::new("kill").arg(pid.to_string()).output();
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn wait_for<T>(what: &str, timeout: Duration, mut f: impl FnMut() -> Option<T>) -> T {
    let end = Instant::now() + timeout;
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(Instant::now() < end, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn up(client: &GatewayClient) {
    wait_for("the gateway link", Duration::from_secs(30), || {
        matches!(client.link(), Link::Up(_)).then_some(())
    });
}

fn controllers(client: &GatewayClient) -> Vec<ControllerInfo> {
    match client
        .query(Query::Topic(Topic::Controllers))
        .wait(Duration::from_secs(10))
    {
        Some(Ok(Answer::Topic(v))) => match *v {
            TopicValue::Controllers(c) => c,
            other => panic!("controllers value: {other:?}"),
        },
        other => panic!("controllers query: {other:?}"),
    }
}

fn screen_text(client: &GatewayClient, info: &ControllerInfo) -> String {
    match client
        .query(Query::Screen(info.target.clone()))
        .wait(Duration::from_secs(10))
    {
        Some(Ok(Answer::Screen(s))) => s.lines.join("\n"),
        other => panic!("screen query: {other:?}"),
    }
}

#[test]
fn headless_gateway_controls_real_omp_without_a_gui() {
    if which("omp").is_none() {
        eprintln!("skipped: omp is not installed");
        return;
    }
    let sb = Sandbox::new();
    let ns = sb.ns.clone();
    let gw = |extra: &[&str]| -> Vec<String> {
        let mut args = vec!["--ns".to_owned(), ns.clone()];
        args.extend(extra.iter().map(|s| (*s).to_owned()));
        args
    };

    // Not running yet.
    assert_eq!(sb.gateway(&gw(&["status"])).status.code(), Some(3));

    // Concurrent starts: every one succeeds, exactly one owner serves.
    let starts: Vec<_> = (0..6)
        .map(|_| {
            let args = gw(&["serve"]);
            std::thread::spawn(move || Command::new(GATEWAY).args(args).output().expect("serve"))
        })
        .collect();
    for s in starts {
        let out = s.join().expect("serve thread");
        assert!(out.status.success(), "serve: {out:?}");
    }
    let first_owner = wait_for("the owner", Duration::from_secs(15), || sb.owner_pid());
    let already = sb.gateway(&gw(&["serve"]));
    assert!(already.status.success());
    assert!(
        String::from_utf8_lossy(&already.stdout).contains(&format!("pid={first_owner}")),
        "a later start must report the one owner: {already:?}"
    );

    // A GUI-role client: no GUI process involved, only the IPC.
    let client = sb.client();
    up(&client);
    let epoch1 = client.epoch().expect("epoch");

    // Launch a real omp session in a sandbox project.
    let launch = client.mutate(Mutation::Launch(LaunchSpec {
        mux_name: Some("omp-new-gwe2e".into()),
        cwd: sb.proj(),
        session_path: None,
        label: "e2e".into(),
        argv: vec!["omp".into()],
        role: SessionRole::Session,
        copy_of: None,
    }));
    launch
        .accepted
        .wait(Duration::from_secs(20))
        .expect("accepted in time")
        .expect("launch accepted");
    let target = match launch.outcome.wait(Duration::from_secs(60)) {
        Some(Ok(Outcome::Done(d))) => match *d {
            Done::Session(t) => t,
            other => panic!("launch result: {other:?}"),
        },
        other => panic!("launch outcome: {other:?}"),
    };
    assert_eq!(target.epoch, epoch1);

    // omp comes up idle with live control.
    let idle = |c: &ControllerInfo| {
        c.target == target && c.live_control && !c.busy && c.exit_code.is_none()
    };
    let deadline = Instant::now() + Duration::from_secs(90);
    let info = loop {
        let all = controllers(&client);
        if let Some(c) = all.iter().find(|c| idle(c)) {
            break c.clone();
        }
        if Instant::now() > deadline {
            let screens: Vec<String> = all.iter().map(|c| screen_text(&client, c)).collect();
            panic!("no idle live-control session: {all:#?}\nscreens: {screens:#?}");
        }
        std::thread::sleep(Duration::from_millis(250));
    };
    wait_for("omp's composer on screen", Duration::from_secs(60), || {
        let text = screen_text(&client, &info);
        (!text.trim().is_empty()).then_some(())
    });

    // Real command: /rename, through the controller's injection (delivery and runtime verdicts).
    let title = "gw-e2e-title";
    let rename = client.submit(
        target.clone(),
        ControlCommand::Action(Action::Rename(title.into())),
    );
    rename
        .accepted
        .wait(Duration::from_secs(10))
        .expect("accepted")
        .expect("rename accepted");
    match rename.outcome.wait(Duration::from_secs(60)) {
        Some(Ok(Outcome::Done(d))) => match *d {
            Done::Submitted(rec) => {
                assert!(
                    rec.delivery == Delivery::Delivered,
                    "delivery: {:?}",
                    rec.delivery
                );
                assert!(
                    rec.runtime == Runtime::Untracked,
                    "runtime: {:?}",
                    rec.runtime
                );
            }
            other => panic!("rename result: {other:?}"),
        },
        other => panic!("rename outcome: {other:?}"),
    }
    // omp applied it: the terminal title the gateway observed carries the new name.
    wait_for("the renamed title", Duration::from_secs(30), || {
        controllers(&client)
            .into_iter()
            .find(|c| c.target == target && c.title.as_deref().is_some_and(|t| t.contains(title)))
            .map(|_| ())
    });

    // The owner is lost (SIGKILL): the backend session survives and a new owner adopts it.
    drop(client);
    let out = Command::new("kill")
        .args(["-9", &first_owner.to_string()])
        .output()
        .expect("kill");
    assert!(out.status.success());
    wait_for("the old owner to be gone", Duration::from_secs(10), || {
        (sb.owner_pid() != Some(first_owner)).then_some(())
    });

    let client = sb.client();
    up(&client);
    let epoch2 = client.epoch().expect("epoch");
    assert_ne!(epoch1, epoch2, "a new owner is a new epoch");
    let adopted = wait_for(
        "the live session re-adopted",
        Duration::from_secs(60),
        || {
            controllers(&client)
                .into_iter()
                .find(|c| c.label == "e2e" && c.exit_code.is_none())
        },
    );
    assert_eq!(adopted.target.epoch, epoch2);
    assert!(
        screen_text(&client, &adopted).contains("omp")
            || !screen_text(&client, &adopted).trim().is_empty(),
        "the re-adopted session shows its screen"
    );

    // Stop/kill are this session only.
    let kill = client.mutate(Mutation::Kill(adopted.target.clone()));
    kill.accepted
        .wait(Duration::from_secs(10))
        .expect("accepted")
        .expect("kill accepted");
    match kill.outcome.wait(Duration::from_secs(30)) {
        Some(Ok(Outcome::Done(_))) => {}
        other => panic!("kill outcome: {other:?}"),
    }
    wait_for("the process to end", Duration::from_secs(30), || {
        let list = controllers(&client);
        (!list
            .iter()
            .any(|c| c.label == "e2e" && c.exit_code.is_none()))
        .then_some(())
    });
}

fn which(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(program))
        .find(|p| p.is_file())
        .or_else(|| {
            // omp installs to ~/.local/bin, which a sandboxed HOME hides.
            let real = std::env::var_os("REAL_HOME")
                .map(PathBuf::from)
                .or_else(|| dirs_home().map(|h| h.join(".local/bin")))?;
            let p = real.join(program);
            p.is_file().then_some(p)
        })
}

fn dirs_home() -> Option<PathBuf> {
    // The sandbox replaced HOME; the passwd entry is not needed because PATH normally has omp.
    None
}
