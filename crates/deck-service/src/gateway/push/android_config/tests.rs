use super::*;

fn input() -> Value {
    json!({
        "project_info": {
            "project_id": "ompdeck-test",
            "project_number": "123456789012",
            "storage_bucket": "ompdeck-test.firebasestorage.app",
        },
        "client": [{
            "client_info": {
                "mobilesdk_app_id": "1:123456789012:android:0123456789abcdef",
                "android_client_info": {"package_name": PACKAGE},
            },
            "api_key": [{"current_key": "AIzaSyA-0123456789abcdefghijklmnopqrstuv"}],
        }],
    })
}

fn parse(value: &Value) -> Result<String, &'static str> {
    normalize(&serde_json::to_vec(value).unwrap(), Some("ompdeck-test"))
}

#[test]
fn only_the_selected_public_fields_are_published() {
    let expected = input();
    let mut source = expected.clone();
    source["type"] = "service_account".into();
    source["private_key"] = "PRIVATE ACCOUNT MATERIAL".into();
    source["client_email"] = "private-account@example.com".into();
    source["credentials_path"] = "/home/private/firebase-key.json".into();
    source["project_info"]["private_key"] = "PROJECT SECRET".into();
    source["client"][0]["client_info"]["private_key"] = "CLIENT SECRET".into();
    source["client"][0]["api_key"][0]["private_key"] = "API SECRET".into();
    source["client"][0]["oauth_client"] = json!([{"client_secret": "OAUTH SECRET"}]);
    source["client"][0]["api_key"]
        .as_array_mut()
        .unwrap()
        .insert(0, json!({"current_key": "invalid"}));
    source["client"][0]["api_key"]
        .as_array_mut()
        .unwrap()
        .push(json!({"current_key": "OTHER_VALID_KEY_0123456789"}));
    let mut other = expected["client"][0].clone();
    other["client_info"]["android_client_info"]["package_name"] = "com.other.app".into();
    other["private_key"] = "OTHER APP SECRET".into();
    source["client"].as_array_mut().unwrap().insert(0, other);
    let output = parse(&source).unwrap();
    assert_eq!(serde_json::from_str::<Value>(&output).unwrap(), expected);
    for forbidden in [
        "PRIVATE",
        "SECRET",
        "private_key",
        "client_email",
        "/home/",
        "credentials_path",
    ] {
        assert!(!output.contains(forbidden));
    }
}

#[test]
fn sender_and_public_project_must_match() {
    let bytes = serde_json::to_vec(&input()).unwrap();
    assert!(normalize(&bytes, None).is_err());
    assert!(normalize(&bytes, Some("another-project")).is_err());
    assert!(normalize(&bytes, Some("ompdeck-test")).is_ok());
    // A service account on its own is never public client configuration.
    assert!(
        normalize(
            br#"{"type":"service_account","project_id":"ompdeck-test","private_key":"secret"}"#,
            Some("ompdeck-test")
        )
        .is_err()
    );
}

#[test]
fn malformed_or_spliced_android_fields_are_rejected() {
    let invalid: &[(&[&str], Value)] = &[
        (&["project_info", "project_id"], json!("Ompdeck-test")),
        (&["project_info", "project_id"], json!("bad-")),
        (&["project_info", "project_number"], json!("12345")),
        (
            &["project_info", "project_number"],
            json!("123456789012345678901"),
        ),
        (&["project_info", "project_number"], json!("123456x")),
        (
            &["project_info", "storage_bucket"],
            json!("https://private.example/bucket"),
        ),
    ];
    for (path, value) in invalid {
        let mut source = input();
        source[path[0]][path[1]] = value.clone();
        assert!(parse(&source).is_err(), "{path:?}");
    }
    for app in [
        "1:999999999999:android:0123456789abcdef",
        "1:123456789012:ios:0123456789abcdef",
        "1:123456789012:android:ABCDEF0123456789",
        "1:123456789012:android:0123456",
        "1:123456789012:android:0123456789abcdef:extra",
    ] {
        let mut source = input();
        source["client"][0]["client_info"]["mobilesdk_app_id"] = app.into();
        assert!(parse(&source).is_err());
    }
    for key in [
        "x".repeat(19),
        "x".repeat(129),
        "has-invalid/slash-key".into(),
    ] {
        let mut source = input();
        source["client"][0]["api_key"][0]["current_key"] = key.into();
        assert!(parse(&source).is_err());
    }
    let mut source = input();
    source["client"][0]["client_info"]["android_client_info"]["package_name"] =
        "other.package".into();
    assert!(parse(&source).is_err());
    for source in [
        json!({}),
        json!(null),
        json!({"project_info": false}),
        json!({"project_info": []}),
    ] {
        assert!(parse(&source).is_err());
    }
}

#[test]
fn size_bound_is_in_bytes_and_accepts_the_exact_limit() {
    let mut bytes = serde_json::to_vec(&input()).unwrap();
    bytes.resize(MAX_BYTES, b' ');
    assert!(normalize(&bytes, Some("ompdeck-test")).is_ok());
    bytes.push(b' ');
    assert!(normalize(&bytes, Some("ompdeck-test")).is_err());
    assert!(normalize(b"invalid JSON", Some("ompdeck-test")).is_err());
    // A multibyte payload below the character limit can still exceed the byte limit.
    let mut source = input();
    source["ignored"] = "\u{e9}".repeat(MAX_BYTES / 2).into();
    let bytes = serde_json::to_vec(&source).unwrap();
    assert!(bytes.len() > MAX_BYTES);
    assert!(normalize(&bytes, Some("ompdeck-test")).is_err());
}

#[test]
fn optional_bucket_and_valid_key_boundaries_are_supported() {
    for length in [20, 128] {
        let mut source = input();
        source["project_info"]
            .as_object_mut()
            .unwrap()
            .remove("storage_bucket");
        source["client"][0]["api_key"][0]["current_key"] = "a".repeat(length).into();
        let output: Value = serde_json::from_str(&parse(&source).unwrap()).unwrap();
        assert_eq!(output, source);
    }
}

#[test]
fn startup_loading_obeys_override_bounds_and_sender_readiness() {
    let dir = tempfile::tempdir().unwrap();
    let sender = fcm::Config::with_endpoints(
        fcm::tests::creds(),
        fcm::Endpoints::loopback(0, "ompdeck-test"),
    );
    assert!(load_from(dir.path(), None, Some(&sender)).is_none());
    let default = dir.path().join(FILE);
    std::fs::write(&default, serde_json::to_vec(&input()).unwrap()).unwrap();
    assert!(load_from(dir.path(), None, Some(&sender)).is_some());
    assert!(load_from(dir.path(), None, None).is_none());
    let explicit = dir.path().join("public-client.json");
    assert!(load_from(dir.path(), Some(explicit.clone().into()), Some(&sender)).is_none());
    std::fs::write(&explicit, b"{}").unwrap();
    assert!(load_from(dir.path(), Some(explicit.clone().into()), Some(&sender)).is_none());
    let mut wrong_project = input();
    wrong_project["project_info"]["project_id"] = "different-project".into();
    std::fs::write(&explicit, serde_json::to_vec(&wrong_project).unwrap()).unwrap();
    assert!(load_from(dir.path(), Some(explicit.clone().into()), Some(&sender)).is_none());
    std::fs::write(&explicit, serde_json::to_vec(&input()).unwrap()).unwrap();
    assert!(load_from(dir.path(), Some(explicit.clone().into()), Some(&sender)).is_some());
    std::fs::write(&explicit, vec![b' '; MAX_BYTES + 1]).unwrap();
    assert!(load_from(dir.path(), Some(explicit.into()), Some(&sender)).is_none());
}
