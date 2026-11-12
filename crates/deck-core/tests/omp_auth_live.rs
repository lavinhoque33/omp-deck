//! Real omp authentication, using only sandbox credentials (vllm's empty local key) and a
//! cancelled Anthropic browser flow. No browser is launched and no real account is logged in.
//! Opt in with OMPDECK_TEST_OMP=<omp binary>.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use deck_core::omp::{LoginEvent, LoginRun, Omp};

struct Sandbox {
    _root: tempfile::TempDir,
    omp: Omp,
}

impl Sandbox {
    fn new() -> Option<Self> {
        let Some(executable) = std::env::var_os("OMPDECK_TEST_OMP").map(PathBuf::from) else {
            eprintln!("skipping: OMPDECK_TEST_OMP is not set");
            return None;
        };
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let agent = home.join(".omp/agent");
        std::fs::create_dir_all(&agent).unwrap();
        std::fs::write(agent.join("config.yml"),
            "startup:\n  setupWizard: false\ncollab:\n  autoStart: off\ndisabledProviders:\n  - deepseek\n").unwrap();
        let mut omp = Omp::at(executable)
            .with_env("HOME", &home)
            .with_env("USERPROFILE", &home)
            .with_env("PI_CODING_AGENT_DIR", &agent)
            .with_env("PI_CONFIG_DIR", ".omp")
            .with_env("PI_NOTIFICATIONS", "off");
        for (key, subdir) in [
            ("XDG_CONFIG_HOME", ".config"),
            ("XDG_DATA_HOME", ".local/share"),
            ("XDG_STATE_HOME", ".local/state"),
            ("XDG_CACHE_HOME", ".cache"),
        ] {
            omp = omp.with_env(key, home.join(subdir));
        }
        Some(Self { _root: root, omp })
    }
}

fn next_events(run: &mut LoginRun, deadline: Instant) -> Vec<LoginEvent> {
    loop {
        let events = run.poll();
        if !events.is_empty() {
            return events;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for a login event"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn providers_local_key_logout_and_cancelled_oauth_use_omps_own_storage() {
    let Some(sandbox) = Sandbox::new() else {
        return;
    };
    let providers = sandbox.omp.login_providers().unwrap();
    assert!(providers.len() > 50, "{} providers", providers.len());
    assert!(providers.iter().any(|p| p.id == "anthropic"));
    assert_eq!(
        providers
            .iter()
            .find(|p| p.id == "zai-coding-plan")
            .unwrap()
            .store,
        "zai"
    );
    assert!(
        providers
            .iter()
            .find(|p| p.id == "deepseek")
            .unwrap()
            .disabled,
        "cfgDisabledProviders.get(settings) must read the isolated config"
    );
    assert!(
        providers
            .iter()
            .find(|p| p.id == "vllm")
            .unwrap()
            .accounts
            .is_empty()
    );
    eprintln!(
        "listed {} providers; zai alias and disabled settings verified",
        providers.len()
    );

    let mut login = sandbox.omp.login("vllm").unwrap();
    let deadline = Instant::now() + Duration::from_secs(25);
    let mut auth_seen = false;
    let mut prompt_seen = false;
    let mut complete = false;
    while !complete {
        for event in next_events(&mut login, deadline) {
            match event {
                LoginEvent::Auth { .. } => auth_seen = true,
                LoginEvent::Prompt {
                    seq, allow_empty, ..
                } => {
                    assert!(auth_seen, "vllm must report Auth before Prompt");
                    assert!(allow_empty);
                    prompt_seen = true;
                    login.answer(seq, "").unwrap();
                }
                LoginEvent::Done { store, .. } => {
                    assert!(prompt_seen);
                    assert_eq!(store, "vllm");
                    complete = true;
                }
                LoginEvent::Failed { message, .. } => panic!("local login failed: {message}"),
                _ => {}
            }
        }
    }
    assert!(
        login.poll().is_empty(),
        "terminal events are delivered once"
    );
    drop(login);
    let providers = sandbox.omp.login_providers().unwrap();
    let vllm = providers.iter().find(|p| p.id == "vllm").unwrap();
    assert_eq!(vllm.accounts.len(), 1);
    assert_eq!(vllm.accounts[0].kind, "api_key");
    let credential_id = vllm.accounts[0].credential_id;
    let logout = sandbox.omp.logout("vllm", credential_id).unwrap();
    assert!(logout.removed);
    let providers = sandbox.omp.login_providers().unwrap();
    assert!(
        providers
            .iter()
            .find(|p| p.id == "vllm")
            .unwrap()
            .accounts
            .is_empty()
    );
    eprintln!("vllm empty-key login, persisted account, and logout verified");

    let mut login = sandbox.omp.login("anthropic").unwrap();
    let deadline = Instant::now() + Duration::from_secs(25);
    let mut cancel_started = None;
    loop {
        let limit = cancel_started.map_or(deadline, |started: Instant| {
            started + Duration::from_secs(5)
        });
        for event in next_events(&mut login, limit) {
            match event {
                LoginEvent::Auth {
                    url, launch_url, ..
                } => {
                    assert_eq!(url.split('/').nth(2), Some("claude.ai"), "{url}");
                    assert!(launch_url.is_some());
                    cancel_started = Some(Instant::now());
                    login.cancel();
                }
                LoginEvent::Failed { cancelled, message } => {
                    assert!(cancelled, "{message}");
                    let elapsed = cancel_started
                        .expect("Auth must precede cancellation")
                        .elapsed();
                    assert!(elapsed < Duration::from_secs(5), "{elapsed:?}");
                    eprintln!(
                        "Anthropic claude.ai URL + launch_url verified; cancelled in {elapsed:?}"
                    );
                    return;
                }
                LoginEvent::Done { .. } => panic!("cancelled OAuth must not log in"),
                _ => {}
            }
        }
    }
}
