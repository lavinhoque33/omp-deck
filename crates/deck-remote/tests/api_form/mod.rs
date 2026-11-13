//! The HTML-form additions against `tests/vectors/api-form.json` (`docs/notes/contracts.md`,
//! "omp 18.18 HTML-form `ask` on the phone"), which the PWA tests read too: a live session listing
//! a form request with every block and field kind, every form answer shape, the welcome listing
//! `form`, and the new request refusals. Same format as `api-s4.json`. Regenerated with the v1
//! file: `OMPDECK_WRITE_VECTORS=1 cargo test -p deck-remote --test api`

use super::*;

pub const PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/vectors/api-form.json");

fn s(text: &str) -> String {
    text.to_owned()
}

fn plain(text: &str) -> UiSpan {
    UiSpan {
        text: s(text),
        style: None,
    }
}

fn styled(text: &str, style: SpanStyle) -> UiSpan {
    UiSpan {
        text: s(text),
        style: Some(style),
    }
}

fn option(label: &str, checked: bool, disabled: bool) -> UiFormOption {
    UiFormOption {
        label: vec![plain(label)],
        checked,
        disabled,
    }
}

pub fn requests() -> Vec<(&'static str, Request)> {
    let answer = |id: u64, value: UiAnswer| Request {
        id,
        seq: Some(id),
        op: Op::AnswerUi {
            live: live(),
            request: s("u-21"),
            value,
        },
    };
    vec![
        (
            "answer form",
            answer(
                301,
                UiAnswer::Form {
                    fields: vec![
                        UiFormAnswer::Choice {
                            checked: vec![1],
                            other: None,
                        },
                        UiFormAnswer::Choice {
                            checked: vec![0, 2],
                            other: None,
                        },
                        UiFormAnswer::Choice {
                            checked: vec![0],
                            other: None,
                        },
                        UiFormAnswer::Text { text: s("3") },
                        UiFormAnswer::Text {
                            text: s("dev@example.com"),
                        },
                        UiFormAnswer::Text {
                            text: s("line 1\nline 2"),
                        },
                    ],
                },
            ),
        ),
        (
            "answer form with an own text",
            answer(
                302,
                UiAnswer::Form {
                    fields: vec![
                        UiFormAnswer::Choice {
                            checked: vec![],
                            other: Some(s("teal")),
                        },
                        UiFormAnswer::Choice {
                            checked: vec![],
                            other: None,
                        },
                        UiFormAnswer::Choice {
                            checked: vec![],
                            other: None,
                        },
                        UiFormAnswer::Text {
                            text: String::new(),
                        },
                        UiFormAnswer::Text {
                            text: String::new(),
                        },
                        UiFormAnswer::Text {
                            text: String::new(),
                        },
                    ],
                },
            ),
        ),
        (
            "answer form without fields",
            answer(303, UiAnswer::Form { fields: vec![] }),
        ),
    ]
}

/// A form with every block kind, span style and field kind.
pub fn form() -> UiFormView {
    UiFormView {
        sections: vec![
            UiFormSectionView {
                title: vec![],
                blocks: vec![
                    UiFormBlock::Text {
                        spans: vec![plain("Release plan")],
                        heading: true,
                    },
                    UiFormBlock::Text {
                        spans: vec![
                            plain("Ship "),
                            styled("v2", SpanStyle::Code),
                            plain(" "),
                            styled("today", SpanStyle::Strong),
                            plain(", "),
                            styled("maybe", SpanStyle::Em),
                            plain(".\nSecond line"),
                        ],
                        heading: false,
                    },
                    UiFormBlock::List {
                        items: vec![
                            vec![plain("parser")],
                            vec![styled("ctl.rs", SpanStyle::Code)],
                        ],
                        ordered: false,
                    },
                    UiFormBlock::List {
                        items: vec![vec![plain("build")], vec![plain("test")]],
                        ordered: true,
                    },
                    UiFormBlock::Pre {
                        text: s("fn main() {\n    run();\n}"),
                    },
                    UiFormBlock::Table {
                        rows: vec![
                            vec![vec![plain("Crate")], vec![plain("Tests")]],
                            vec![
                                vec![styled("deck-core", SpanStyle::Code)],
                                vec![plain("212")],
                            ],
                        ],
                        header_rows: 1,
                    },
                    UiFormBlock::Image {
                        alt: s("architecture diagram"),
                    },
                ],
            },
            UiFormSectionView {
                title: vec![plain("Choices")],
                blocks: vec![
                    UiFormBlock::Field { index: 0 },
                    UiFormBlock::Field { index: 1 },
                    UiFormBlock::Field { index: 2 },
                ],
            },
            UiFormSectionView {
                title: vec![plain("Details "), styled("(optional)", SpanStyle::Em)],
                blocks: vec![
                    UiFormBlock::Field { index: 3 },
                    UiFormBlock::Field { index: 4 },
                    UiFormBlock::Field { index: 5 },
                ],
            },
        ],
        fields: vec![
            UiFormFieldView::Choice {
                name: s("color"),
                question: s("Pick a color?"),
                label: vec![plain("Pick a color?")],
                multi: false,
                select: false,
                other: true,
                required: true,
                options: vec![
                    option("red", false, false),
                    option("blue", true, false),
                    option("green", false, true),
                ],
            },
            UiFormFieldView::Choice {
                name: s("extras"),
                question: s("Extras"),
                label: vec![plain("Extras")],
                multi: true,
                select: false,
                other: false,
                required: false,
                options: vec![
                    option("docs", true, false),
                    option("bench", false, false),
                    option("changelog", false, false),
                ],
            },
            UiFormFieldView::Choice {
                name: s("target"),
                question: s("Target"),
                label: vec![plain("Target")],
                multi: false,
                select: true,
                other: false,
                required: false,
                options: vec![
                    option("linux", true, false),
                    option("windows", false, false),
                ],
            },
            UiFormFieldView::Text {
                name: s("retries"),
                question: s("Retries"),
                label: vec![plain("Retries")],
                input_type: s("number"),
                multiline: false,
                value: s("2"),
                placeholder: String::new(),
                required: true,
                min: Some(s("0")),
                max: Some(s("7.5")),
                pattern: None,
                min_length: None,
                max_length: None,
                rows: 1,
            },
            UiFormFieldView::Text {
                name: s("mail"),
                question: s("Mail"),
                label: vec![plain("Mail")],
                input_type: s("email"),
                multiline: false,
                value: String::new(),
                placeholder: s("you@example.com"),
                required: false,
                min: None,
                max: None,
                pattern: Some(s(".+@example\\.com")),
                min_length: Some(5),
                max_length: Some(80),
                rows: 1,
            },
            UiFormFieldView::Text {
                name: s("notes"),
                question: s("Notes"),
                label: vec![plain("Notes")],
                input_type: s("textarea"),
                multiline: true,
                value: String::new(),
                placeholder: s("Anything else"),
                required: false,
                min: None,
                max: None,
                pattern: None,
                min_length: None,
                max_length: Some(2000),
                rows: 4,
            },
        ],
    }
}

fn events() -> Vec<(&'static str, Event)> {
    let live_session = LiveSession {
        live: live(),
        kind: LiveKind::Session,
        label: s("Plan the release"),
        title: Some(s("Plan the release")),
        session: Some(SESSION.into()),
        project: Some(PROJECT.into()),
        display: s("~/src/app"),
        activity: Activity::Attention,
        status: Status::Attention {},
        exit_code: None,
        busy: false,
        model: None,
        pending_switch: None,
        live_control: true,
        desktop_controlled: false,
        views: 0,
        requests: vec![],
        progress: None,
        pending_ui: vec![UiRequestView {
            title: s("Ask"),
            timeout_ms: Some(60_000),
            form: Some(form()),
            ..s4::ui("u-21", UiKind::Form)
        }],
    };
    vec![(
        "feed live with a form request",
        Event::Feed {
            feed: Feed::Live {},
            value: FeedValue::Live {
                sessions: vec![live_session],
                truncated: false,
            },
        },
    )]
}

fn welcomes() -> Vec<(&'static str, Welcome)> {
    vec![(
        "welcome with s4 and form",
        Welcome {
            protocol: PROTOCOL,
            gateway_version: s("0.27.0"),
            epoch: s("00112233445566778899aabbccddeeff"),
            device: s("d-0123456789abcdef"),
            name: s("Pixel"),
            permissions: Permissions {
                control: true,
                manage: false,
            },
            next_seq: 304,
            limits: Limits::default(),
            features: vec![FEATURE_S4.into(), FEATURE_FORM.into()],
            push_key: String::new(),
        },
    )]
}

/// `(case, json, expected)`: form request bodies a gateway refuses.
fn rejected_requests() -> Vec<(&'static str, &'static str, Value)> {
    vec![
        (
            "unknown form answer type",
            r#"{"id":311,"seq":311,"op":{"op":"answer_ui","live":{"key":"k","generation":1},"request":"u-1","value":{"type":"form","fields":[{"type":"text","text":"x"},{"type":"slider","value":3}]}}}"#,
            json!({"error": "unsupported", "id": 311}),
        ),
        (
            "unknown field in a form answer",
            r#"{"id":312,"seq":312,"op":{"op":"answer_ui","live":{"key":"k","generation":1},"request":"u-1","value":{"type":"form","fields":[{"type":"choice","checked":[0],"other":null,"value":"red"}]}}}"#,
            json!({"error": "bad_request", "id": 312}),
        ),
        (
            "unknown field in a form value",
            r#"{"id":313,"seq":313,"op":{"op":"answer_ui","live":{"key":"k","generation":1},"request":"u-1","value":{"type":"form","fields":[],"data":{}}}}"#,
            json!({"error": "bad_request", "id": 313}),
        ),
        (
            "form choice without checked",
            r#"{"id":314,"seq":314,"op":{"op":"answer_ui","live":{"key":"k","generation":1},"request":"u-1","value":{"type":"form","fields":[{"type":"choice","other":"teal"}]}}}"#,
            json!({"error": "bad_request", "id": 314}),
        ),
    ]
}

fn messages() -> Vec<Value> {
    let mut out = Vec::new();
    let mut push = |kind: &str, case: &str, text: String| {
        out.push(json!({"type": kind, "case": case, "json": text}));
    };
    for (case, v) in requests() {
        push("request", case, json_text(&v));
    }
    for (case, v) in events() {
        push("event", case, json_text(&v));
    }
    for (case, v) in welcomes() {
        push("welcome", case, json_text(&v));
    }
    out
}

fn build() -> Value {
    let requests: Vec<Value> = rejected_requests()
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
        "description": "HTML-form additions to the device application protocol (deck_remote::api, docs/notes/contracts.md \"omp 18.18 HTML-form ask on the phone\") shared with pwa/src/remote: sent only when both sides announce the features listed here. messages: the canonical JSON text of a live session listing a form request (every block, span style and field kind), every form answer shape and the welcome listing form (decode equals the value, encode equals the text; None is null except UiRequestView.form, which is omitted when absent); rejected_requests: REQUEST bodies answered unsupported/bad_request (by id). Records and framing are unchanged (api-v1.json). Generated by crates/deck-remote/tests/api_form/mod.rs.",
        "protocol": PROTOCOL,
        "features": [FEATURE_S4, FEATURE_FORM],
        "messages": messages(),
        "rejected_requests": requests,
    })
}

#[test]
fn form_vectors_match() {
    let built = build();
    let text = serde_json::to_string_pretty(&built).unwrap() + "\n";
    if std::env::var_os("OMPDECK_WRITE_VECTORS").is_some() {
        std::fs::write(PATH, &text).unwrap();
    }
    let file = std::fs::read_to_string(PATH).expect("api-form.json (OMPDECK_WRITE_VECTORS=1)");
    assert!(
        file == text,
        "api-form.json drifted: OMPDECK_WRITE_VECTORS=1"
    );
}

#[test]
fn form_messages_decode_to_their_value_and_encode_to_their_text() {
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
    check(&by_type["event"], events());
    check(&by_type["welcome"], welcomes());
    for (case, r) in requests() {
        let text = serde_json::to_vec(&r).unwrap();
        assert!(Request::parse(&text).is_ok(), "{case}");
        assert!(r.check_seq().is_ok(), "{case}");
        assert!(r.op.check().is_ok(), "{case}");
    }
}

#[test]
fn form_rejected_requests_are_classified() {
    for (case, text, expect) in rejected_requests() {
        let got = Request::parse(text.as_bytes()).unwrap_err();
        assert_eq!(request_error_name(&got), expect["error"], "{case}");
        assert_eq!(request_error_id(&got), expect["id"].as_u64(), "{case}");
    }
}

/// A request without `form` (every pre-form message) decodes to `None` and encodes without it;
/// a choice answer may leave out `other`.
#[test]
fn form_is_omitted_when_absent_and_other_is_optional() {
    let ui = s4::ui("u-1", UiKind::Select);
    let text = json_text(&ui);
    assert!(!text.contains("form"), "{text}");
    let decoded: UiRequestView = serde_json::from_str(&text).unwrap();
    assert_eq!(decoded, ui);
    let answer: UiFormAnswer = serde_json::from_str(r#"{"type":"choice","checked":[2]}"#).unwrap();
    assert_eq!(
        answer,
        UiFormAnswer::Choice {
            checked: vec![2],
            other: None,
        }
    );
}
