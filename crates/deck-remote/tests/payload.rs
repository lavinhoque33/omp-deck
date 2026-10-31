#![cfg(feature = "noise")]
//! Handshake payload acceptance against `tests/vectors/payload-v1.json` (the grammar) and
//! `tests/vectors/hello-v1.json` (display-safe text, `api`, `features`, versions), which the PWA
//! tests (`pwa/test/payload.test.ts`) check too: both sides accept and reject exactly the same
//! bytes, with the same error kind.

use deck_remote::noise::{DeviceHello, NoiseError, Welcome, is_display_safe};
use serde_json::Value;
use sha2::{Digest, Sha256};

const VECTORS: &str = include_str!("vectors/payload-v1.json");
const HELLO_VECTORS: &str = include_str!("vectors/hello-v1.json");
const README: &str = include_str!("vectors/README.md");

fn unhex(text: &str) -> Vec<u8> {
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).expect("hex"))
        .collect()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The error kind a vector names: `unsupported_version`, else a payload error.
fn check_error(case: &str, error: &NoiseError, expected: Option<&str>) {
    match expected {
        Some("unsupported_version") => {
            assert!(
                matches!(error, NoiseError::UnsupportedVersion),
                "{case}: {error:?}"
            );
        }
        None | Some("payload") => {
            assert!(matches!(error, NoiseError::Payload(_)), "{case}: {error:?}");
        }
        Some(other) => panic!("{case}: unknown error kind {other}"),
    }
}

/// Runs `check` on every vector of `kind`; returns how many there were.
fn each(kind: &str, mut check: impl FnMut(&str, &[u8], Result<&Value, Option<&str>>)) -> usize {
    let file: Value = serde_json::from_str(VECTORS).unwrap();
    let vectors = file[kind].as_array().unwrap();
    for vector in vectors {
        let case = vector["case"].as_str().unwrap();
        let bytes = unhex(vector["hex"].as_str().unwrap());
        let expect = if vector["accept"].as_bool().unwrap() {
            Ok(&vector["expect"])
        } else {
            Err(vector["error"].as_str())
        };
        check(case, &bytes, expect);
    }
    vectors.len()
}

#[test]
fn hello_vectors() {
    let count = each("hello", |case, bytes, expect| {
        match (DeviceHello::decode(bytes), expect) {
            (Ok(hello), Ok(expect)) => {
                assert_eq!(hello.name, expect["name"].as_str().unwrap(), "{case}");
                assert_eq!(
                    hello.platform,
                    expect["platform"].as_str().unwrap(),
                    "{case}"
                );
                assert_eq!(hello.ts, expect["ts"].as_u64().unwrap(), "{case}");
            }
            (Err(error), Err(kind)) => check_error(case, &error, kind),
            (result, _) => panic!("{case}: {result:?}"),
        }
    });
    assert!(count > 40);
}

#[test]
fn welcome_vectors() {
    let count = each("welcome", |case, bytes, expect| {
        match (Welcome::decode(bytes), expect) {
            (Ok(welcome), Ok(expect)) => {
                assert_eq!(welcome.device, expect["device"].as_str().unwrap(), "{case}");
                assert_eq!(
                    welcome.gateway,
                    expect["gateway"].as_str().unwrap(),
                    "{case}"
                );
                assert_eq!(
                    welcome.token.map(|t| hex(&t)).as_deref(),
                    expect["token"].as_str(),
                    "{case}"
                );
            }
            (Err(error), Err(kind)) => check_error(case, &error, kind),
            (result, _) => panic!("{case}: {result:?}"),
        }
    });
    assert!(count > 20);
}

#[test]
fn display_safe_vectors() {
    let file: Value = serde_json::from_str(HELLO_VECTORS).unwrap();
    let vectors = file["display_safe"].as_array().unwrap();
    for vector in vectors {
        let case = vector["case"].as_str().unwrap();
        let text = vector["text"].as_str().unwrap();
        assert_eq!(
            is_display_safe(text),
            vector["safe"].as_bool().unwrap(),
            "{case}"
        );
    }
    // Every listed code point appears.
    assert!(vectors.len() > 130);
}

#[test]
fn hello_field_vectors() {
    let file: Value = serde_json::from_str(HELLO_VECTORS).unwrap();
    let vectors = file["hello"].as_array().unwrap();
    for vector in vectors {
        let case = vector["case"].as_str().unwrap();
        let payload = vector["payload"].as_str().unwrap();
        let decoded = DeviceHello::decode(payload.as_bytes());
        if !vector["accept"].as_bool().unwrap() {
            let error = decoded.expect_err(case);
            check_error(case, &error, vector["error"].as_str());
            continue;
        }
        let hello = decoded.unwrap_or_else(|e| panic!("{case}: {e:?}"));
        let expect = &vector["expect"];
        let features: Vec<&str> = expect["features"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f.as_str().unwrap())
            .collect();
        assert_eq!(hello.name, expect["name"].as_str().unwrap(), "{case}");
        assert_eq!(
            hello.platform,
            expect["platform"].as_str().unwrap(),
            "{case}"
        );
        assert_eq!(hello.ts, expect["ts"].as_u64().unwrap(), "{case}");
        assert_eq!(hello.api, expect["api"].as_u64().unwrap(), "{case}");
        assert_eq!(hello.features, features, "{case}");
        let encoded = hello.encode().unwrap();
        assert_eq!(
            std::str::from_utf8(&encoded).unwrap(),
            vector["encoded"].as_str().unwrap(),
            "{case}"
        );
    }
    assert!(vectors.len() > 40);
}

#[test]
fn encode_refuses_what_decode_refuses() {
    let hello = |name: &str, api: u64, features: &[&str]| DeviceHello {
        name: name.into(),
        platform: "android".into(),
        ts: 5,
        api,
        features: features.iter().map(|f| (*f).to_owned()).collect(),
    };
    let too_many: Vec<String> = (0..33).map(|i| format!("f{i}")).collect();
    let too_many: Vec<&str> = too_many.iter().map(String::as_str).collect();
    for (case, bad) in [
        ("unsafe name", hello("a\u{202e}b", 1, &[])),
        ("api 0", hello("a", 0, &[])),
        ("api 2^53", hello("a", 1 << 53, &[])),
        ("empty feature", hello("a", 1, &[""])),
        ("comma inside a feature", hello("a", 1, &["a,b"])),
        ("duplicate feature", hello("a", 1, &["a", "a"])),
        ("33 features", hello("a", 1, &too_many)),
    ] {
        let error = bad.encode().expect_err(case);
        assert!(matches!(error, NoiseError::Payload(_)), "{case}: {error:?}");
    }
}

/// The SHA-256 the README records for `file`.
fn recorded_sha256(file: &str) -> &'static str {
    let marker = format!("`{file}` SHA-256: `");
    let start = README
        .find(&marker)
        .unwrap_or_else(|| panic!("README.md records no SHA-256 for {file}"))
        + marker.len();
    &README[start..start + 64]
}

#[test]
fn hand_maintained_files_match_their_recorded_sha256() {
    for (file, bytes) in [
        ("payload-v1.json", VECTORS.as_bytes()),
        ("hello-v1.json", HELLO_VECTORS.as_bytes()),
        ("invite-v1.json", include_bytes!("vectors/invite-v1.json")),
        (
            "cacophony-ik.json",
            include_bytes!("vectors/cacophony-ik.json"),
        ),
    ] {
        assert_eq!(
            hex(&Sha256::digest(bytes)),
            recorded_sha256(file),
            "{file} changed: an edit of a hand-maintained vector file must update README.md"
        );
    }
}
