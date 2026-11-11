//! The model-picker preference additions against `tests/vectors/api-models.json`, which the PWA
//! and Android tests read too: the desired-state `model_favorite` and `model_provider` requests
//! (no feature gate: an older gateway answers `unsupported`), a `models` reply carrying the shared
//! `favorites` and `provider`, and the new request refusals. Same format as `api-s4.json`.
//! Regenerated with the v1 file: `OMPDECK_WRITE_VECTORS=1 cargo test -p deck-remote --test api`

use super::*;

pub const PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/vectors/api-models.json");

fn model(provider: &str, id: &str, name: &str) -> Model {
    Model {
        selector: format!("{provider}/{id}"),
        provider: provider.into(),
        id: id.into(),
        name: name.into(),
        context_window: Some(200_000),
        max_tokens: None,
        reasoning: Some(true),
        thinking: vec!["low".into(), "high".into()],
        input: vec!["text".into()],
        cost_input: None,
        cost_output: None,
    }
}

fn catalogue() -> Vec<Model> {
    vec![
        model("Anthropic", "claude-sonnet-4", "Claude Sonnet 4"),
        model("anthropic", "claude-haiku-4", "Claude Haiku 4"),
        model("openai", "gpt-5", "GPT-5"),
    ]
}

pub fn requests() -> Vec<(&'static str, Request)> {
    let journaled = |id: u64, op: Op| Request {
        id,
        seq: Some(id),
        op,
    };
    vec![
        (
            "model_favorite on",
            journaled(
                501,
                Op::ModelFavorite {
                    selector: "anthropic/claude-sonnet-4".into(),
                    favorite: true,
                },
            ),
        ),
        (
            "model_favorite off",
            journaled(
                502,
                Op::ModelFavorite {
                    selector: "openai/gpt-5".into(),
                    favorite: false,
                },
            ),
        ),
        (
            "model_provider set",
            journaled(
                503,
                Op::ModelProvider {
                    provider: Some("openai".into()),
                },
            ),
        ),
        (
            "model_provider all",
            journaled(504, Op::ModelProvider { provider: None }),
        ),
    ]
}

pub fn replies() -> Vec<(&'static str, Reply)> {
    let ok = |id: u64, r: Resp| Reply {
        id,
        result: ReplyResult::Ok(r),
    };
    vec![
        (
            "models with favorites and provider",
            ok(
                505,
                Resp::Models {
                    models: catalogue(),
                    truncated: false,
                    favorites: vec!["openai/gpt-5".into(), "gone/retired-1".into()],
                    provider: Some("anthropic".into()),
                },
            ),
        ),
        (
            "models with favorites only",
            ok(
                506,
                Resp::Models {
                    models: catalogue(),
                    truncated: false,
                    favorites: vec!["openai/gpt-5".into()],
                    provider: None,
                },
            ),
        ),
    ]
}

fn rejected_requests() -> Vec<(&'static str, &'static str, Value)> {
    vec![
        (
            "model_favorite without favorite",
            r#"{"id":511,"seq":511,"op":{"op":"model_favorite","selector":"openai/gpt-5"}}"#,
            json!({"error": "bad_request", "id": 511}),
        ),
        (
            "model_favorite toggle is not a thing",
            r#"{"id":512,"seq":512,"op":{"op":"model_favorite","selector":"openai/gpt-5","favorite":true,"toggle":true}}"#,
            json!({"error": "bad_request", "id": 512}),
        ),
        (
            "model_favorite with a non-boolean favorite",
            r#"{"id":513,"seq":513,"op":{"op":"model_favorite","selector":"openai/gpt-5","favorite":"yes"}}"#,
            json!({"error": "bad_request", "id": 513}),
        ),
    ]
}

fn messages() -> Vec<Value> {
    let mut out = Vec::new();
    for (case, v) in requests() {
        out.push(json!({"type": "request", "case": case, "json": json_text(&v)}));
    }
    for (case, v) in replies() {
        out.push(json!({"type": "reply", "case": case, "json": json_text(&v)}));
    }
    out
}

fn build() -> Value {
    let rejected: Vec<Value> = rejected_requests()
        .into_iter()
        .map(|(case, text, expect)| {
            let mut v = json!({"case": case, "json": text});
            if let (Some(o), Some(e)) = (v.as_object_mut(), expect.as_object()) {
                o.extend(e.clone());
            }
            v
        })
        .collect();
    json!({
        "description": "Model-picker preference additions to the device application protocol (deck_remote::api), not feature-gated (an older gateway answers unsupported) and shared with pwa/src/remote and the Android app. messages: model_favorite (explicit desired state, never a toggle), model_provider (null provider = all providers) and a models reply listing the shared favorites (starring order, unavailable ones kept) and provider filter; both fields are omitted when empty/absent, so every older models reply is unchanged (api-v1.json). rejected_requests: REQUEST bodies answered unsupported/bad_request (by id). Generated by crates/deck-remote/tests/api_models/mod.rs.",
        "protocol": PROTOCOL,
        "messages": messages(),
        "rejected_requests": rejected,
    })
}

#[test]
fn model_vectors_match() {
    let built = build();
    let text = serde_json::to_string_pretty(&built).unwrap() + "\n";
    if std::env::var_os("OMPDECK_WRITE_VECTORS").is_some() {
        std::fs::write(PATH, &text).unwrap();
    }
    let file = std::fs::read_to_string(PATH).expect("api-models.json (OMPDECK_WRITE_VECTORS=1)");
    assert!(
        file == text,
        "api-models.json drifted: OMPDECK_WRITE_VECTORS=1"
    );
}

#[test]
fn model_messages_decode_to_their_value_and_encode_to_their_text() {
    let file: Value = serde_json::from_str(&std::fs::read_to_string(PATH).unwrap()).unwrap();
    let mut by_type: std::collections::BTreeMap<&str, Vec<(&str, &str)>> = Default::default();
    for m in file["messages"].as_array().unwrap() {
        by_type
            .entry(m["type"].as_str().unwrap())
            .or_default()
            .push((m["case"].as_str().unwrap(), m["json"].as_str().unwrap()));
    }
    fn check<T>(vectors: &[(&str, &str)], values: Vec<(&str, T)>)
    where
        T: serde::Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug,
    {
        assert_eq!(vectors.len(), values.len());
        for ((case, text), (_, value)) in vectors.iter().zip(values) {
            let decoded: T = serde_json::from_str(text).unwrap_or_else(|e| panic!("{case}: {e}"));
            assert_eq!(decoded, value, "{case}");
            assert_eq!(serde_json::to_string(&value).unwrap(), *text, "{case}");
        }
    }
    check(&by_type["request"], requests());
    check(&by_type["reply"], replies());
    for (case, r) in requests() {
        let text = serde_json::to_vec(&r).unwrap();
        assert_eq!(Request::parse(&text).unwrap(), r, "{case}");
    }
}

#[test]
fn model_rejected_requests_are_classified() {
    for (case, text, expect) in rejected_requests() {
        let got = Request::parse(text.as_bytes()).unwrap_err();
        assert_eq!(request_error_name(&got), expect["error"], "{case}");
        assert_eq!(request_error_id(&got), expect["id"].as_u64(), "{case}");
    }
}

/// Preference writes are Control (journaled, replayable by `seq`), never View or Manage; the
/// strings are bounded.
#[test]
fn model_preferences_are_journaled_control_with_bounded_strings() {
    let favorite = |selector: &str| Op::ModelFavorite {
        selector: selector.into(),
        favorite: true,
    };
    let provider = |p: Option<&str>| Op::ModelProvider {
        provider: p.map(Into::into),
    };
    for op in [favorite("a/b"), provider(Some("a")), provider(None)] {
        assert_eq!(op.class(), Class::Control, "{}", op.name());
        assert!(op.journaled(), "{}", op.name());
    }
    let max = "x".repeat(MAX_MODEL_PREF);
    let over = "x".repeat(MAX_MODEL_PREF + 1);
    assert!(favorite(&max).check().is_ok());
    assert!(favorite("").check().is_err());
    assert!(favorite(&over).check().is_err());
    assert!(provider(Some(&max)).check().is_ok());
    assert!(provider(Some("")).check().is_err());
    assert!(provider(Some(&over)).check().is_err());
    assert!(provider(None).check().is_ok());
    assert_eq!(Op::Models {}.class(), Class::View);
    let unsequenced = Request {
        id: 1,
        seq: None,
        op: favorite("a/b"),
    };
    assert!(unsequenced.check_seq().is_err());
}

/// A `models` reply from before the preferences (no `favorites`, no `provider`) still decodes,
/// to the empty defaults, and the defaults encode back to the same bytes.
#[test]
fn an_older_models_reply_decodes_to_empty_preferences() {
    let old = r#"{"id":8,"ok":{"type":"models","models":[],"truncated":false}}"#;
    let reply: Reply = serde_json::from_str(old).unwrap();
    assert_eq!(
        reply.result,
        ReplyResult::Ok(Resp::Models {
            models: vec![],
            truncated: false,
            favorites: vec![],
            provider: None,
        })
    );
    assert_eq!(serde_json::to_string(&reply).unwrap(), old);
    let with: Reply = serde_json::from_str(
        r#"{"id":8,"ok":{"type":"models","models":[],"truncated":false,"favorites":["a/b"],"provider":"a"}}"#,
    )
    .unwrap();
    assert!(matches!(
        with.result,
        ReplyResult::Ok(Resp::Models { favorites, provider: Some(p), .. }) if favorites == ["a/b"] && p == "a"
    ));
}
