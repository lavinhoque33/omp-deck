//! Session-store helper operations, plugin and usage-reset CLI wrappers against a real omp in a
//! sandbox (`PI_CODING_AGENT_DIR`, HOME and XDG dirs in a temp dir; `startup.setupWizard false`).
//! Runs only when `OMPDECK_TEST_OMP=<path to omp>` is set (omp is not a build dependency).
//! No model is called. Nothing under the real `~/.omp` is read or written.

use std::path::{Path, PathBuf};

use deck_core::omp::{
    AdvisorDoc, AdvisorEntry, AdvisorScope, ForeignSource, Omp, PluginScope, SessError,
};
use deck_core::usage::{AccountPolicy, OAuthIdentity};
use serde_json::{Value, json};

struct Sandbox {
    root: tempfile::TempDir,
    omp: Omp,
    exe: PathBuf,
}

impl Sandbox {
    fn new() -> Option<Sandbox> {
        let Some(exe) = std::env::var_os("OMPDECK_TEST_OMP").map(PathBuf::from) else {
            eprintln!("skipping: OMPDECK_TEST_OMP is not set");
            return None;
        };
        let root = tempfile::tempdir().expect("tempdir");
        let home = root.path().join("home");
        let agent = home.join(".omp/agent");
        std::fs::create_dir_all(&agent).expect("agent dir");
        std::fs::create_dir_all(home.join("proj")).expect("project dir");
        std::fs::write(
            agent.join("config.yml"),
            "startup:\n  setupWizard: false\ncollab:\n  autoStart: off\n",
        )
        .expect("omp config");
        let mut omp = Omp::at(&exe)
            .with_env("HOME", &home)
            .with_env("USERPROFILE", &home)
            .with_env("PI_CODING_AGENT_DIR", &agent)
            .with_env("PI_NOTIFICATIONS", "off");
        for (k, sub) in [
            ("XDG_CONFIG_HOME", ".config"),
            ("XDG_DATA_HOME", ".local/share"),
            ("XDG_CACHE_HOME", ".cache"),
            ("XDG_STATE_HOME", ".local/state"),
        ] {
            omp = omp.with_env(k, home.join(sub));
        }
        Some(Sandbox { root, omp, exe })
    }

    fn home(&self) -> PathBuf {
        self.root.path().join("home")
    }

    fn agent(&self) -> PathBuf {
        self.home().join(".omp/agent")
    }

    fn proj(&self) -> PathBuf {
        self.home().join("proj")
    }

    /// A v3 session with a user message `m1` and an assistant reply `m2`.
    fn session(&self, id: &str) -> PathBuf {
        let dir = self.agent().join("sessions/--proj--");
        std::fs::create_dir_all(&dir).expect("store dir");
        let file = dir.join(format!("2026-01-01T00-00-00-000Z_{id}.jsonl"));
        // omp's own layout: a `title` line padded to exactly 256 bytes first (omp rewrites it in
        // place, so its width must not change; a file without it is a legacy one, which omp
        // migrates into a continuation file instead of editing in place).
        let mut title = json!({"type":"title","v":1,"title":"","updatedAt":"2026-01-01T00:00:00.000Z","pad":""});
        title["pad"] = " ".repeat(255 - title.to_string().len()).into();
        let lines = [
            title,
            json!({"type":"session","version":3,"id":id,"timestamp":"2026-01-01T00:00:00.000Z","cwd":self.proj()}),
            json!({"type":"message","id":"m1","parentId":null,"timestamp":"2026-01-01T00:00:01.000Z",
                "message":{"role":"user","content":"hi","timestamp":1}}),
            json!({"type":"message","id":"m2","parentId":"m1","timestamp":"2026-01-01T00:00:02.000Z",
                "message":{"role":"assistant","content":[{"type":"text","text":"hello"}],"api":"openai-completions",
                "provider":"fake","model":"fake-1","stopReason":"stop","timestamp":2,
                "usage":{"input":1,"output":1,"cacheRead":0,"cacheWrite":0,"totalTokens":2,
                "cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}}}}),
        ];
        let text: Vec<String> = lines.iter().map(Value::to_string).collect();
        std::fs::write(&file, text.join("\n") + "\n").expect("session file");
        file
    }
}

fn entries(file: &Path) -> Vec<Value> {
    std::fs::read_to_string(file)
        .expect("read session")
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

#[test]
fn pins_round_trip_through_omp_and_the_pin_reader() {
    let Some(sb) = Sandbox::new() else { return };
    // A custom sessions root can differ from the headless omp's default agent directory.
    let store = sb.root.path().join("custom-agent");
    std::fs::create_dir(&store).unwrap();
    sb.omp.set_pinned(&store, "sess-a", true).unwrap();
    sb.omp.set_pinned(&store, "sess-b", true).unwrap();
    sb.omp.set_pinned(&store, "sess-a", true).unwrap(); // already pinned: no-op
    assert!(deck_core::pins::read(&sb.agent()).is_empty());
    let pins = deck_core::pins::read(&store);
    assert!(pins.contains("sess-a") && pins.contains("sess-b"));
    sb.omp.set_pinned(&store, "sess-a", false).unwrap();
    let pins = deck_core::pins::read(&store);
    assert_eq!(pins.into_iter().collect::<Vec<_>>(), ["sess-b"]);
}

#[test]
fn closed_sessions_are_renamed_and_labelled_in_place() {
    let Some(sb) = Sandbox::new() else { return };
    let file = sb.session("sess0001");

    let stored = sb.omp.rename_session(&file, "  Fix the parser  ").unwrap();
    assert_eq!(stored, "Fix the parser", "omp cleans the title it stores");
    let rows = entries(&file);
    let title = rows.iter().find(|e| e["type"] == "title_change").unwrap();
    assert_eq!(
        (title["title"].as_str(), title["source"].as_str()),
        (Some("Fix the parser"), Some("user"))
    );

    sb.omp.set_label(&file, "m1", Some("start")).unwrap();
    let label = |rows: &[Value]| {
        rows.iter()
            .rev()
            .find(|e| e["type"] == "label")
            .map(|e| (e["targetId"].clone(), e["label"].clone()))
    };
    assert_eq!(label(&entries(&file)), Some((json!("m1"), json!("start"))));
    sb.omp.set_label(&file, "m1", None).unwrap();
    assert_eq!(
        label(&entries(&file)),
        Some((json!("m1"), Value::Null)),
        "removing a label appends an entry without one"
    );

    let before = std::fs::read_to_string(&file).unwrap();
    match sb.omp.set_label(&file, "nope", Some("x")) {
        Err(SessError::Failed(msg)) => assert!(msg.contains("nope"), "{msg}"),
        other => panic!("an unknown entry is omp's error, got {other:?}"),
    }
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        before,
        "a refused label writes nothing"
    );
}

#[test]
fn foreign_sessions_are_listed_and_imported() {
    let Some(sb) = Sandbox::new() else { return };
    let id = "aaaa1111-2222-3333-4444-555566667777";
    let enc: String = sb
        .proj()
        .to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let dir = sb.home().join(".claude/projects").join(enc);
    std::fs::create_dir_all(&dir).unwrap();
    let cwd = sb.proj();
    let rows = [
        json!({"type":"user","uuid":"u1","parentUuid":null,"cwd":cwd,"sessionId":id,
            "timestamp":"2026-01-01T00:00:00.000Z","message":{"role":"user","content":"hello claude"}}),
        json!({"type":"assistant","uuid":"a1","parentUuid":"u1","cwd":cwd,"sessionId":id,
            "timestamp":"2026-01-01T00:00:01.000Z",
            "message":{"role":"assistant","model":"claude-sonnet-4","content":[{"type":"text","text":"hi there"}]}}),
    ];
    let text: Vec<String> = rows.iter().map(Value::to_string).collect();
    std::fs::write(dir.join(format!("{id}.jsonl")), text.join("\n") + "\n").unwrap();

    assert!(
        sb.omp
            .foreign_sessions(ForeignSource::Codex)
            .unwrap()
            .is_empty()
    );
    let listed = sb.omp.foreign_sessions(ForeignSource::Claude).unwrap();
    assert_eq!(listed.len(), 1, "{listed:?}");
    assert_eq!(
        (listed[0].source, listed[0].id.as_str()),
        (ForeignSource::Claude, id)
    );
    assert_eq!(Path::new(&listed[0].cwd), cwd);

    let imported = sb
        .omp
        .import_foreign(ForeignSource::Claude, id, &cwd)
        .unwrap();
    assert!(
        imported.starts_with(sb.agent().join("sessions")),
        "{}",
        imported.display()
    );
    let rows = entries(&imported);
    let header = &rows.iter().find(|e| e["type"] == "session").unwrap();
    assert_ne!(header["id"], id, "the import gets a fresh omp session id");
    let record = rows
        .iter()
        .find(|e| e["customType"] == "foreign_session_import")
        .expect("the import is recorded");
    assert_eq!(record["data"]["source"], "claude");
    assert_eq!(record["data"]["sourceId"], id);
    let texts: Vec<String> = rows
        .iter()
        .filter(|e| e["type"] == "message")
        .map(|e| e["message"]["content"].to_string())
        .collect();
    assert!(
        texts.iter().any(|t| t.contains("hello claude"))
            && texts.iter().any(|t| t.contains("hi there"))
    );

    match sb
        .omp
        .import_foreign(ForeignSource::Claude, "no-such-id", &cwd)
    {
        Err(SessError::Failed(_)) => {}
        other => panic!("an unknown foreign id fails, got {other:?}"),
    }
}

#[test]
fn advisor_documents_round_trip_and_an_empty_one_deletes_the_file() {
    let Some(sb) = Sandbox::new() else { return };
    let cwd = sb.proj();
    let none = sb.omp.advisor_config(&cwd, AdvisorScope::Project).unwrap();
    assert!(none.advisors.is_empty() && none.warnings.is_empty());
    let file = none.path.clone().expect("the file a save would write");
    assert_eq!(file, cwd.join("WATCHDOG.yml"));

    let doc = AdvisorDoc {
        instructions: Some("Be terse.\nNo preamble.".into()),
        max_notes_per_update: Some(3),
        extra: serde_json::from_value(json!({"futureTop": [true, null, "s"]})).unwrap(),
        advisors: vec![
            AdvisorEntry {
                name: "Reviewer".into(),
                model: Some("fake/fake-1".into()),
                tools: Some(vec!["read".into(), "grep".into()]),
                review_mode: Some("turn".into()),
                review_interval: Some(2),
                sync_backlog: Some("3".into()),
                instructions: Some("Review diffs.".into()),
                enabled: Some(false),
                max_notes_per_update: Some(1),
                scope: Some("main".into()),
                include_reasoning: Some(true),
                extra: serde_json::from_value(
                    json!({"futureKnob": {"a": [1, "x"]}, "owner": "me"}),
                )
                .unwrap(),
            },
            AdvisorEntry {
                name: "Quiet".into(),
                tools: Some(vec![]),
                ..AdvisorEntry::default()
            },
        ],
        ..AdvisorDoc::default()
    };
    let saved = sb
        .omp
        .save_advisor_config(&cwd, AdvisorScope::Project, &doc)
        .unwrap();
    assert_eq!(
        (saved.path.as_path(), saved.removed),
        (file.as_path(), false)
    );
    let yaml = std::fs::read_to_string(&file).unwrap();
    assert!(
        yaml.contains("name: \"Reviewer\"") || yaml.contains("name: Reviewer"),
        "{yaml}"
    );
    let back = sb.omp.advisor_config(&cwd, AdvisorScope::Project).unwrap();
    assert_eq!(
        AdvisorDoc {
            path: None,
            ..back.clone()
        },
        doc,
        "what was saved is what is read"
    );

    // Scopes are separate files; the user one lives in the agent dir.
    let user = sb.omp.advisor_config(&cwd, AdvisorScope::User).unwrap();
    assert_eq!(
        user.path.as_deref(),
        Some(sb.agent().join("WATCHDOG.yml").as_path())
    );
    assert!(user.advisors.is_empty());

    // An invalid entry is dropped with a warning; the file is not an error.
    std::fs::write(
        &file,
        "advisors:\n  - name: Ok\n  - name: Bad\n    reviewMode: sometimes\n  - 7\n",
    )
    .unwrap();
    let partial = sb.omp.advisor_config(&cwd, AdvisorScope::Project).unwrap();
    assert_eq!(
        partial
            .advisors
            .iter()
            .map(|a| a.name.as_str())
            .collect::<Vec<_>>(),
        ["Ok"]
    );
    assert_eq!(partial.warnings.len(), 2, "{:?}", partial.warnings);

    let removed = sb
        .omp
        .save_advisor_config(&cwd, AdvisorScope::Project, &AdvisorDoc::default())
        .unwrap();
    assert!(removed.removed && !file.exists());
}

#[test]
fn plugins_list_toggle_and_configure() {
    let Some(sb) = Sandbox::new() else { return };
    assert!(sb.omp.plugins().unwrap().npm.is_empty());

    let pkg = sb.home().join("myplug");
    std::fs::create_dir_all(&pkg).unwrap();
    std::fs::write(
        pkg.join("package.json"),
        // Raw text: `json!` would sort the keys, and the order is what is checked.
        r#"{"name":"deck-test-plugin","version":"1.2.3","description":"A test plugin",
            "omp":{"features":{"extra":{"description":"Extra stuff","default":true},"beta":{"description":"Beta"}},
            "settings":{"apiKey":{"type":"string","secret":true,"description":"Key"},
                "retries":{"type":"number","min":1,"max":9,"default":3},
                "verbose":{"type":"boolean","default":false},
                "mode":{"type":"enum","values":["fast","slow"],"default":"fast"}}}}"#,
    )
    .unwrap();
    // `plugin link` has no wrapper (OmpDeck does not link plugins): run it directly.
    let mut link = std::process::Command::new(&sb.exe);
    link.args(["plugin", "link", "./myplug"])
        .current_dir(sb.home());
    for (k, v) in [("HOME", sb.home()), ("PI_CODING_AGENT_DIR", sb.agent())] {
        link.env(k, v);
    }
    for (k, sub) in [
        ("XDG_CONFIG_HOME", ".config"),
        ("XDG_DATA_HOME", ".local/share"),
        ("XDG_CACHE_HOME", ".cache"),
        ("XDG_STATE_HOME", ".local/state"),
    ] {
        link.env(k, sb.home().join(sub));
    }
    assert!(link.output().unwrap().status.success(), "plugin link");

    let list = sb.omp.plugins().unwrap();
    let plugin = &list.npm[0];
    assert_eq!(
        (
            plugin.name.as_str(),
            plugin.version.as_str(),
            plugin.enabled
        ),
        ("deck-test-plugin", "1.2.3", true)
    );
    assert_eq!(
        plugin.enabled_features, None,
        "no explicit choice: the defaults apply"
    );
    let features: Vec<&str> = plugin
        .manifest
        .features
        .iter()
        .map(|(k, _)| k.as_str())
        .collect();
    assert_eq!(features, ["extra", "beta"], "manifest order is kept");
    let settings: Vec<&str> = plugin
        .manifest
        .settings
        .iter()
        .map(|(k, _)| k.as_str())
        .collect();
    assert_eq!(settings, ["apiKey", "retries", "verbose", "mode"]);

    sb.omp
        .plugin_set_enabled("deck-test-plugin", false, PluginScope::User)
        .unwrap();
    assert!(!sb.omp.plugins().unwrap().npm[0].enabled);
    sb.omp
        .plugin_set_enabled("deck-test-plugin", true, PluginScope::User)
        .unwrap();
    assert!(sb.omp.plugins().unwrap().npm[0].enabled);
    assert!(
        sb.omp
            .plugin_set_enabled("nope", true, PluginScope::User)
            .is_err()
    );

    let state = sb
        .omp
        .plugin_features("deck-test-plugin", &["beta".to_owned()])
        .unwrap();
    assert_eq!(
        state.enabled_features.as_deref(),
        Some(&["beta".to_owned()][..])
    );
    assert_eq!(state.available_features, ["extra", "beta"]);
    let none = sb.omp.plugin_features("deck-test-plugin", &[]).unwrap();
    assert_eq!(
        none.enabled_features.as_deref(),
        Some(&[][..]),
        "an empty list disables every feature"
    );

    sb.omp
        .plugin_config_set("deck-test-plugin", "retries", &json!(5))
        .unwrap();
    sb.omp
        .plugin_config_set("deck-test-plugin", "apiKey", &json!("sekret"))
        .unwrap();
    let cfg = sb.omp.plugin_config("deck-test-plugin").unwrap();
    assert_eq!(cfg.settings["retries"], json!(5));
    assert_ne!(
        cfg.settings["apiKey"],
        json!("sekret"),
        "secrets never leave the wrapper in clear"
    );
    assert!(
        sb.omp
            .plugin_config_set("deck-test-plugin", "retries", &json!(99))
            .is_err(),
        "omp validates ranges"
    );
    assert!(
        sb.omp
            .plugin_config_set("deck-test-plugin", "mode", &json!("warp"))
            .is_err(),
        "omp validates enums"
    );
    sb.omp
        .plugin_config_delete("deck-test-plugin", "retries")
        .unwrap();
    assert!(
        !sb.omp
            .plugin_config("deck-test-plugin")
            .unwrap()
            .settings
            .contains_key("retries")
    );
}

#[test]
fn usage_resets_of_a_store_without_accounts_is_empty_and_spending_is_refused() {
    let Some(sb) = Sandbox::new() else { return };
    let list = sb.omp.usage_resets(None).unwrap();
    assert!(list.accounts.is_empty());
    assert!(list.generated_at > 0);
    assert!(
        sb.omp
            .usage_resets(Some("anthropic"))
            .unwrap()
            .accounts
            .is_empty()
    );
    let err = sb
        .omp
        .usage_reset_spend("openai-codex", "1")
        .unwrap_err()
        .to_string();
    assert!(err.contains("No stored account"), "{err}");
}

const ACCOUNT_GLOBAL: &str = include_str!("fixtures/omp/accounts-global.yml");
const ACCOUNT_PROJECT: &str = include_str!("fixtures/omp/accounts-project.yml");
const CODEX: &str = "openai-codex";

fn account(id: &str) -> OAuthIdentity {
    OAuthIdentity::new(None, Some(id), None, None).unwrap()
}

fn effective_policies(sb: &Sandbox, cwd: &Path) -> Value {
    let out = std::process::Command::new(&sb.exe)
        .args(["config", "get", "auth.accountPolicies", "--json"])
        .current_dir(cwd)
        .env("HOME", sb.home())
        .env("USERPROFILE", sb.home())
        .env("PI_CODING_AGENT_DIR", sb.agent())
        .env_remove("PI_CONFIG_FILES")
        .env("XDG_CONFIG_HOME", sb.home().join(".config"))
        .env("XDG_DATA_HOME", sb.home().join(".local/share"))
        .env("XDG_CACHE_HOME", sb.home().join(".cache"))
        .env("XDG_STATE_HOME", sb.home().join(".local/state"))
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice::<Value>(&out.stdout).unwrap()["value"].take()
}

#[test]
fn prefer_account_preserves_order_fields_and_native_scope() {
    let Some(sb) = Sandbox::new() else { return };
    let global_file = sb.agent().join("config.yml");
    let project = sb.proj();
    let project_file = project.join(".omp/config.yml");
    let sibling = sb.home().join("sibling");
    std::fs::write(&global_file, ACCOUNT_GLOBAL).unwrap();
    std::fs::create_dir_all(project.join(".omp")).unwrap();
    std::fs::write(&project_file, ACCOUNT_PROJECT).unwrap();
    std::fs::create_dir(&sibling).unwrap();

    let global_before = effective_policies(&sb, &sibling);
    assert_eq!(effective_policies(&sb, &project), global_before);
    let full_identity = OAuthIdentity::new(
        Some("second@example.invalid"),
        Some("second"),
        None,
        Some("team-b"),
    )
    .unwrap();
    let project_policies = sb
        .omp
        .prefer_account(CODEX, &full_identity, Some(&project))
        .unwrap();
    let mut promoted = global_before[2].clone();
    promoted["priority"] = json!(20);
    assert_eq!(
        project_policies,
        json!([
            promoted,
            global_before[0],
            global_before[1],
            global_before[3]
        ])
    );
    assert_eq!(effective_policies(&sb, &project), project_policies);
    assert_eq!(
        std::fs::read_to_string(&global_file).unwrap(),
        ACCOUNT_GLOBAL
    );
    let raw_project = std::fs::read_to_string(&project_file).unwrap();
    assert!(raw_project.contains("usageReservePct: 12"), "{raw_project}");
    assert!(
        raw_project.contains("unpausedProviders: []"),
        "{raw_project}"
    );
    assert!(raw_project.contains("customExtension:"), "{raw_project}");

    let saved_global = sb.omp.prefer_account(CODEX, &account("new"), None).unwrap();
    assert_eq!(
        saved_global[0],
        json!({"provider": CODEX, "account": {"accountId": "new"}, "priority": 20})
    );
    assert_eq!(
        &saved_global.as_array().unwrap()[1..],
        global_before.as_array().unwrap().as_slice()
    );
    assert_eq!(effective_policies(&sb, &sibling), saved_global);
    assert_eq!(effective_policies(&sb, &project), project_policies);
    assert_eq!(std::fs::read_to_string(&project_file).unwrap(), raw_project);

    // A project with existing policies keeps editing its snapshot, not the new global array.
    let promoted_again = sb
        .omp
        .prefer_account(CODEX, &account("first"), Some(&project))
        .unwrap();
    let mut first = global_before[0].clone();
    first["priority"] = json!(21);
    assert_eq!(
        promoted_again,
        json!([
            first,
            project_policies[0],
            project_policies[2],
            project_policies[3]
        ])
    );
    assert_eq!(effective_policies(&sb, &project), promoted_again);
    assert_eq!(effective_policies(&sb, &sibling), saved_global);

    // First preference in a project with no .omp yet uses the new global snapshot.
    assert!(!sibling.join(".omp").exists());
    let sibling_policies = sb
        .omp
        .prefer_account("anthropic", &account("other"), Some(&sibling))
        .unwrap();
    assert_eq!(sibling_policies.as_array().unwrap().len(), 6);
    assert_eq!(sibling_policies[2]["account"]["accountId"], "other");
    assert_eq!(effective_policies(&sb, &sibling), sibling_policies);
    assert_eq!(effective_policies(&sb, &project), promoted_again);
}

#[test]
fn prefer_account_and_order_editor_both_update_only_the_global_layer() {
    let Some(sb) = Sandbox::new() else { return };
    std::fs::write(sb.agent().join("config.yml"), ACCOUNT_GLOBAL).unwrap();
    let overlay = sb.home().join("overlay.yml");
    let overlay_text =
        "auth:\n  accountPolicies: [{provider: overlay-only, account: {accountId: x}}]\n";
    std::fs::write(&overlay, overlay_text).unwrap();
    let omp = sb.omp.clone().with_env("PI_CONFIG_FILES", &overlay);
    let other = AccountPolicy {
        provider: "anthropic".into(),
        account: account("advanced-edit"),
        priority: Some(51),
        reserve_pct: None,
        reclaim_above_pct: None,
    };
    std::thread::scope(|s| {
        let prefer = s.spawn(|| omp.prefer_account(CODEX, &account("click-preferred"), None));
        let edit = omp.set_account_policies("anthropic", std::slice::from_ref(&other));
        prefer.join().unwrap().unwrap();
        edit.unwrap();
    });

    let actual = effective_policies(&sb, &sb.proj());
    assert_eq!(actual[0]["account"]["accountId"], "click-preferred");
    assert_eq!(actual[0]["priority"], 20);
    assert_eq!(actual[2]["reservePct"], 15);
    assert_eq!(actual[2]["reclaimAbovePct"], 55);
    assert_eq!(actual[4], serde_json::to_value(other).unwrap());
    assert_eq!(actual.as_array().unwrap().len(), 5);
    assert_eq!(std::fs::read_to_string(overlay).unwrap(), overlay_text);
}

#[test]
fn prefer_account_refuses_invalid_project_yaml_without_touching_it() {
    let Some(sb) = Sandbox::new() else { return };
    let dir = sb.proj().join(".omp");
    let file = dir.join("config.yml");
    std::fs::create_dir(&dir).unwrap();
    let broken = "auth: [missing end";
    std::fs::write(&file, broken).unwrap();
    let result = sb
        .omp
        .prefer_account(CODEX, &account("first"), Some(&sb.proj()));
    assert!(result.unwrap_err().to_string().contains("invalid YAML"));
    assert_eq!(std::fs::read_to_string(file).unwrap(), broken);
}
