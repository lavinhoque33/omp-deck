//! Runs the real `omp` binary, always against a throwaway agent dir (`PI_CODING_AGENT_DIR` is set
//! per command through `Omp::with_env`; the test process env is never touched).

use std::fs;
use std::path::{Path, PathBuf};

use deck_core::omp::{MoveKind, Omp, SettingKind};
use serde_json::json;
use std::sync::{Mutex, MutexGuard};

/// omp's live-writer check is global by session id (across agent dirs), so tests that
/// pack/unpack the shared fixture session must not run concurrently.
static SESSION_LOCK: Mutex<()> = Mutex::new(());

fn session_lock() -> MutexGuard<'static, ()> {
    SESSION_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

fn sandboxed(agent: &Path) -> Option<Omp> {
    match Omp::detect() {
        Some(o) => Some(o.with_env("PI_CODING_AGENT_DIR", agent)),
        None => {
            eprintln!("omp not found; skipping real-omp integration test");
            None
        }
    }
}

fn setting<'a>(cat: &'a deck_core::omp::SettingsCatalog, key: &str) -> &'a deck_core::omp::Setting {
    cat.tabs
        .iter()
        .flat_map(|t| &t.settings)
        .find(|s| s.key == key)
        .unwrap_or_else(|| panic!("missing {key}"))
}

#[test]
fn set_and_reset_round_trip_in_sandbox() {
    let tmp = tempfile::tempdir().unwrap();
    let Some(omp) = sandboxed(&tmp.path().join("agent")) else {
        return;
    };
    let cat = omp.load_settings().unwrap();
    let cases = [
        ("autocompleteMaxVisible", json!(-5)),
        ("theme.dark", json!("  spaced  ")),
        ("symbolPreset", json!("nerd")),
        ("compaction.enabled", json!(false)),
        ("enabledModels", json!(["a/b", "c"])),
        ("modelRoles", json!({"default": "x/y"})),
    ];
    for (key, value) in &cases {
        let s = setting(&cat, key);
        assert_eq!(&omp.set_setting(s, value).unwrap(), value, "{key}");
    }
    let after = omp.load_settings().unwrap();
    for (key, value) in &cases {
        let s = setting(&after, key);
        assert_eq!(s.value.as_ref(), Some(value), "{key}");
        assert!(s.is_modified(), "{key}");
    }
    for (key, _) in &cases {
        omp.reset_setting(key).unwrap();
    }
    let reset = omp.load_settings().unwrap();
    for (key, _) in &cases {
        assert!(!setting(&reset, key).is_modified(), "{key}");
    }
}

#[test]
fn secret_set_returns_null_and_is_flagged() {
    let tmp = tempfile::tempdir().unwrap();
    let Some(omp) = sandboxed(&tmp.path().join("agent")) else {
        return;
    };
    let cat = omp.load_settings().unwrap();
    let tok = setting(&cat, "searxng.token");
    assert!(tok.secret && !tok.is_modified());
    assert_eq!(
        omp.set_setting(tok, &json!("sandbox-only-token")).unwrap(),
        json!(null)
    );
    let after = omp.load_settings().unwrap();
    let tok = setting(&after, "searxng.token");
    assert!(tok.secret && tok.is_modified());
    assert!(!format!("{tok:?}").contains("sandbox-only-token"));
}

#[test]
fn invalid_value_is_rejected_with_omp_message() {
    let tmp = tempfile::tempdir().unwrap();
    let Some(omp) = sandboxed(&tmp.path().join("agent")) else {
        return;
    };
    let cat = omp.load_settings().unwrap();
    let s = setting(&cat, "autocompleteMaxVisible");
    assert_eq!(s.kind, SettingKind::Number);
    // Bypass the pre-spawn validation to see omp's own error text through `Failed`.
    let mut forged = s.clone();
    forged.kind = SettingKind::String;
    let err = omp.set_setting(&forged, &json!("abc")).unwrap_err();
    assert_eq!(err.to_string(), "Invalid number: abc");
}

#[test]
fn move_session_relocates_and_rewrites_header_cwd() {
    let _guard = session_lock();
    let tmp = tempfile::tempdir().unwrap();
    let agent = tmp.path().join("agent");
    let Some(omp) = sandboxed(&agent) else {
        return;
    };
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/omp/session/2026-01-01T10-00-00-000Z_0198aa00bb11.jsonl");
    let src_dir = agent.join("sessions/-src");
    fs::create_dir_all(&src_dir).unwrap();
    let src = src_dir.join(fixture.file_name().unwrap());
    fs::copy(&fixture, &src).unwrap();
    let dest = tmp.path().join("dest-project");
    fs::create_dir(&dest).unwrap();

    let out = omp.move_session(&src, &dest).unwrap();
    assert_eq!(out.kind, MoveKind::Replaced);
    assert!(!src.exists(), "original must be gone");
    assert!(out.new_path.starts_with(agent.join("sessions")));
    let text = fs::read_to_string(&out.new_path).unwrap();
    // omp may put a title record first; the header is the `session` record.
    let header = text
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find(|v| v["type"] == "session")
        .unwrap();
    let cwd = header["cwd"].as_str().unwrap();
    assert_eq!(
        fs::canonicalize(cwd).unwrap(),
        fs::canonicalize(&dest).unwrap()
    );
    assert_eq!(header["id"], "0198aa00bb11");
}

#[test]
fn move_session_to_missing_dir_fails_and_keeps_original() {
    let _guard = session_lock();
    let tmp = tempfile::tempdir().unwrap();
    let agent = tmp.path().join("agent");
    let Some(omp) = sandboxed(&agent) else {
        return;
    };
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/omp/session/2026-01-01T10-00-00-000Z_0198aa00bb11.jsonl");
    let src_dir = agent.join("sessions/-src");
    fs::create_dir_all(&src_dir).unwrap();
    let src = src_dir.join(fixture.file_name().unwrap());
    fs::copy(&fixture, &src).unwrap();
    let err = omp
        .move_session(&src, &tmp.path().join("does-not-exist"))
        .unwrap_err();
    assert!(err.to_string().contains("Directory not found"), "{err}");
    assert!(src.exists());
}

// ---- fork_at -----------------------------------------------------------------------------------

fn entry(id: &str, parent: Option<&str>, role: &str, text: &str) -> String {
    let parent = parent.map_or("null".to_owned(), |p| format!("\"{p}\""));
    format!(
        r#"{{"type":"message","id":"{id}","parentId":{parent},"timestamp":"2026-03-01T10:00:0{}.000Z","message":{{"role":"{role}","content":[{{"type":"text","text":"{text}"}}],"timestamp":1}}}}"#,
        id.len() % 9
    )
}

/// A session under `<agent>/sessions/<dir>/` with header cwd `cwd`: model_change m0 → user u1 → assistant a1 →
/// user u2 → assistant a2.
fn write_session(agent: &Path, cwd: &Path) -> PathBuf {
    let dir = agent.join("sessions/-fork-proj");
    fs::create_dir_all(&dir).unwrap();
    let file = dir.join("2026-03-01T10-00-00-000Z_01a00000-0000-7000-8000-000000000001.jsonl");
    let mut s = String::new();
    s.push_str(r#"{"type":"title","v":1,"title":"fork src","source":"user","updatedAt":"2026-03-01T10:00:00.000Z","pad":""}"#);
    s.push('\n');
    s.push_str(&format!(
        r#"{{"type":"session","version":3,"id":"01a00000-0000-7000-8000-000000000001","timestamp":"2026-03-01T10:00:00.000Z","cwd":{}}}"#,
        serde_json::to_string(&cwd.to_string_lossy()).unwrap()
    ));
    s.push('\n');
    s.push_str(
        r#"{"type":"model_change","id":"m0","parentId":null,"timestamp":"2026-03-01T10:00:00.000Z","model":"fake/fake-1","role":"default"}"#,
    );
    s.push('\n');
    for l in [
        entry("u1", Some("m0"), "user", "first"),
        entry("a1", Some("u1"), "assistant", "one"),
        entry("u2", Some("a1"), "user", "second"),
        entry("a2", Some("u2"), "assistant", "two"),
    ] {
        s.push_str(&l);
        s.push('\n');
    }
    fs::write(&file, s).unwrap();
    // Resuming restores the session's model: give the sandbox a (never contacted) one.
    fs::write(
        agent.join("models.yml"),
        "providers:\n  fake:\n    baseUrl: http://127.0.0.1:9/v1\n    apiKey: sk-fake\n    api: openai-completions\n    models:\n      - id: fake-1\n        name: Fake One\n        contextWindow: 100000\n        maxTokens: 4096\n",
    )
    .unwrap();
    fs::write(
        agent.join("config.yml"),
        "startup:\n  setupWizard: false\nmodelRoles:\n  default: fake/fake-1\n",
    )
    .unwrap();
    file
}

fn entry_ids(file: &Path) -> Vec<String> {
    fs::read_to_string(file)
        .unwrap()
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|v| v["type"] == "message")
        .map(|v| v["id"].as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn fork_at_creates_a_new_file_ending_at_the_entry_and_keeps_the_source() {
    let tmp = tempfile::tempdir().unwrap();
    let agent = tmp.path().join("agent");
    let cwd = tmp.path().join("proj");
    fs::create_dir_all(&cwd).unwrap();
    let Some(omp) = sandboxed(&agent) else {
        return;
    };
    let omp = omp.with_env("PI_CONFIG_DIR", tmp.path().join("cfg"));
    let src = write_session(&agent, &cwd);
    let before = fs::read(&src).unwrap();

    let new = omp.fork_at(&src, "a1").unwrap();
    assert_ne!(new, src);
    assert!(new.is_file(), "{}", new.display());
    assert_eq!(entry_ids(&new), ["u1", "a1"]);
    assert_eq!(fs::read(&src).unwrap(), before, "source untouched");

    let err = omp.fork_at(&src, "no-such-entry").unwrap_err();
    assert!(!err.to_string().is_empty());
}

#[cfg(unix)]
#[test]
fn fork_at_speaks_the_rpc_protocol_and_surfaces_failures() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().join("proj");
    fs::create_dir_all(&cwd).unwrap();
    let src = write_session(&tmp.path().join("agent"), &cwd);
    let script = |name: &str, body: &str| {
        let p = tmp.path().join(name);
        fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
        Omp::at(p)
    };

    // success: answers fork then get_state; the new file is reported relative to the process cwd
    let ok = script(
        "ok.sh",
        r#"echo '{"type":"ready"}'
read a; read b
echo "$a" > "$(pwd)/request1.json"
echo '{"id":"1","type":"response","command":"fork","success":true,"data":{"cancelled":false}}'
echo "{\"id\":\"2\",\"type\":\"response\",\"command\":\"get_state\",\"success\":true,\"data\":{\"sessionFile\":\"$(pwd)/forked.jsonl\"}}"
cat >/dev/null"#,
    );
    let got = ok.fork_at(&src, "a1").unwrap();
    assert_eq!(got, cwd.join("forked.jsonl"), "runs in the header cwd");
    let req: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(cwd.join("request1.json")).unwrap()).unwrap();
    assert_eq!(
        (req["type"].as_str(), req["entryId"].as_str()),
        (Some("fork"), Some("a1"))
    );

    // rpc-level failure keeps omp's text
    let locked = script(
        "locked.sh",
        r#"read a
echo '{"id":"1","type":"response","command":"fork","success":false,"error":"Error: session is held by pid 4242"}'
cat >/dev/null"#,
    );
    let e = locked.fork_at(&src, "a1").unwrap_err().to_string();
    assert_eq!(e, "session is held by pid 4242");

    // cancelled fork
    let cancelled = script(
        "cancelled.sh",
        r#"read a
echo '{"id":"1","type":"response","command":"fork","success":true,"data":{"cancelled":true}}'
cat >/dev/null"#,
    );
    assert!(
        cancelled
            .fork_at(&src, "a1")
            .unwrap_err()
            .to_string()
            .contains("cancelled")
    );

    // the process dies at startup (e.g. lease held): stderr is the message
    let dies = script(
        "dies.sh",
        "echo 'Error: session locked by a live omp' >&2\nexit 1",
    );
    let e = dies.fork_at(&src, "a1").unwrap_err().to_string();
    assert_eq!(e, "session locked by a live omp");
}
