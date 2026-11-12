//! Provider-login answers take the transient wire path, never the mutation journal.
use deck_service::gateway::Profile;
use deck_service::gateway::client::{GatewayClient, GatewayOptions};
use deck_service::gateway::protocol::{
    Answer, Done, Mutation, Outcome, Query, Role, SecretText, Topic,
};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

struct Owner(Child);
impl Drop for Owner {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
#[test]
fn transient_login_answer_never_enters_journal() {
    let temp = tempfile::Builder::new()
        .prefix("od-auth-")
        .tempdir()
        .expect("sandbox");
    let profile = Profile {
        host_namespace: format!("omp-deck-test-auth-{}", std::process::id()),
        settings: temp.path().join("settings.json"),
        sessions_root: temp.path().join("agent/sessions"),
    };
    let mut command = Command::new(env!("CARGO_BIN_EXE_omp-deck-gateway"));
    for (key, subdir) in [
        ("HOME", "home"),
        ("USERPROFILE", "home"),
        ("XDG_CONFIG_HOME", "config"),
        ("XDG_DATA_HOME", "data"),
        ("XDG_STATE_HOME", "state"),
        ("XDG_CACHE_HOME", "cache"),
        ("PI_CODING_AGENT_DIR", "agent"),
        ("APPDATA", "config"),
        ("LOCALAPPDATA", "data"),
    ] {
        let path = temp.path().join(subdir);
        std::fs::create_dir_all(&path).expect("sandbox directory");
        command.env(key, path);
    }
    command.env(
        "OMP_DECK_TMUX_SOCKET",
        format!("omp-deck-test-{}", std::process::id()),
    );
    let _owner = Owner(
        command
            .args(["serve", "--foreground", "--ns", &profile.host_namespace])
            .arg("--settings")
            .arg(&profile.settings)
            .arg("--sessions-root")
            .arg(&profile.sessions_root)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("gateway"),
    );
    let mut options = GatewayOptions::new(profile.clone(), Role::Cli, Arc::new(|| {}));
    options.autostart = false;
    let client = GatewayClient::start(options);
    let end = Instant::now() + Duration::from_secs(20);
    while !matches!(client.link(), deck_service::gateway::client::Link::Up(_)) {
        assert!(
            Instant::now() < end,
            "gateway connection: {:?}",
            client.link()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let done = client
        .mutate(Mutation::InboxClear)
        .outcome
        .wait(Duration::from_secs(5))
        .expect("journaled reply")
        .expect("mutation");
    assert!(matches!(done, Outcome::Done(value) if matches!(*value, Done::Unit)));
    let journal = profile.state_dir().join("journal");
    let before = std::fs::read(&journal).expect("journal file");
    assert!(!before.is_empty());
    let secret = "unique-secret-login-answer-do-not-record";
    client.login_answer(999, 777, SecretText::new(secret));
    // The following request fences the transient answer on this connection.
    let reply = client
        .query(Query::Topic(Topic::Gateway))
        .wait(Duration::from_secs(5))
        .expect("fence reply")
        .expect("fence");
    assert!(matches!(reply, Answer::Topic(_)));
    let after = std::fs::read(&journal).expect("journal after answer");
    assert_eq!(
        after, before,
        "an answer must not even append a journal hash"
    );
    assert!(!String::from_utf8_lossy(&after).contains(secret));
}
