#![cfg(feature = "noise")]
//! Handshake payload acceptance against `tests/vectors/payload-v1.json`, which the PWA tests
//! (`pwa/test/payload.test.ts`) check too: both sides accept and reject exactly the same bytes.

use deck_remote::noise::{DeviceHello, NoiseError, Welcome};
use serde_json::Value;

const VECTORS: &str = include_str!("vectors/payload-v1.json");

fn unhex(text: &str) -> Vec<u8> {
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).expect("hex"))
        .collect()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Runs `check` on every vector of `kind`; returns how many there were.
fn each(kind: &str, mut check: impl FnMut(&str, &[u8], Option<&Value>)) -> usize {
    let file: Value = serde_json::from_str(VECTORS).unwrap();
    let vectors = file[kind].as_array().unwrap();
    for vector in vectors {
        let case = vector["case"].as_str().unwrap();
        let bytes = unhex(vector["hex"].as_str().unwrap());
        let expect = vector["accept"]
            .as_bool()
            .unwrap()
            .then(|| &vector["expect"]);
        check(case, &bytes, expect);
    }
    vectors.len()
}

#[test]
fn hello_vectors() {
    let count = each("hello", |case, bytes, expect| {
        match (DeviceHello::decode(bytes), expect) {
            (Ok(hello), Some(expect)) => {
                assert_eq!(hello.name, expect["name"].as_str().unwrap(), "{case}");
                assert_eq!(
                    hello.platform,
                    expect["platform"].as_str().unwrap(),
                    "{case}"
                );
                assert_eq!(hello.ts, expect["ts"].as_u64().unwrap(), "{case}");
            }
            (Err(NoiseError::Payload(_)), None) => {}
            (result, _) => panic!("{case}: {result:?}"),
        }
    });
    assert!(count > 40);
}

#[test]
fn welcome_vectors() {
    let count = each("welcome", |case, bytes, expect| {
        match (Welcome::decode(bytes), expect) {
            (Ok(welcome), Some(expect)) => {
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
            (Err(NoiseError::Payload(_)), None) => {}
            (result, _) => panic!("{case}: {result:?}"),
        }
    });
    assert!(count > 20);
}
