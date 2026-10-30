#![cfg(feature = "noise")]
//! Pairing invite acceptance against `tests/vectors/invite-v1.json`, which the PWA tests
//! (`pwa/test/invite.test.ts`) check too: the canonical relay URL grammar and whole invite texts.

use deck_remote::noise::{NoiseError, PairingInvite};
use serde_json::Value;
use zeroize::Zeroizing;

const VECTORS: &str = include_str!("vectors/invite-v1.json");

fn unhex<const N: usize>(text: &str) -> [u8; N] {
    std::array::from_fn(|i| u8::from_str_radix(&text[2 * i..2 * i + 2], 16).expect("hex"))
}

fn vectors() -> Value {
    serde_json::from_str(VECTORS).unwrap()
}

fn invite(fields: &Value, url: &str) -> PairingInvite {
    PairingInvite {
        url: url.to_owned(),
        room: unhex(fields["room"].as_str().unwrap()),
        token: unhex(fields["token"].as_str().unwrap()),
        gateway_public: unhex(fields["gateway_public"].as_str().unwrap()),
        psk: Zeroizing::new(unhex(fields["psk"].as_str().unwrap())),
    }
}

#[test]
fn url_vectors() {
    let file = vectors();
    let fields = &file["fields"];
    let urls = file["urls"].as_array().unwrap();
    for vector in urls {
        let case = vector["case"].as_str().unwrap();
        let expected = invite(fields, vector["url"].as_str().unwrap());
        let text = vector["invite"].as_str();
        if vector["accept"].as_bool().unwrap() {
            let text = text.unwrap();
            assert_eq!(expected.encode().unwrap(), text, "{case}");
            assert_eq!(PairingInvite::parse(text).unwrap(), expected, "{case}");
        } else {
            let error = expected.encode().expect_err(case);
            assert!(matches!(error, NoiseError::Invite(_)), "{case}: {error:?}");
            if let Some(text) = text {
                let error = PairingInvite::parse(text).expect_err(case);
                assert!(matches!(error, NoiseError::Invite(_)), "{case}: {error:?}");
            }
        }
    }
    assert!(urls.len() > 90);
}

#[test]
fn invite_vectors() {
    let file = vectors();
    let fields = &file["fields"];
    for vector in file["invites"].as_array().unwrap() {
        let case = vector["case"].as_str().unwrap();
        let text = vector["text"].as_str().unwrap();
        match PairingInvite::parse(text) {
            Ok(parsed) => {
                assert!(vector["accept"].as_bool().unwrap(), "{case}");
                assert_eq!(parsed, invite(fields, &parsed.url), "{case}");
                assert_eq!(parsed.encode().unwrap(), text, "{case}");
            }
            Err(error) => {
                assert!(!vector["accept"].as_bool().unwrap(), "{case}: {error:?}");
                assert!(matches!(error, NoiseError::Invite(_)), "{case}: {error:?}");
            }
        }
    }
}
