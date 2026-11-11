//! Native Android's additive subscription contract; the phone decoder reads the same vector.
use super::*;

#[test]
fn native_subscription_shared_vector() {
    let file: Value = serde_json::from_str(include_str!("../vectors/api-android.json")).unwrap();
    let text = file["messages"][0]["json"].as_str().unwrap();
    let request = Request::parse(text.as_bytes()).unwrap();
    assert_eq!(
        request,
        Request {
            id: 401,
            seq: None,
            op: Op::PushAndroidSubscribe {
                token: "test-fcm-registration-token".into(),
                key: "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8".into(),
                desktop: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
            },
        }
    );
    assert_eq!(serde_json::to_string(&request).unwrap(), text);
    assert_eq!(request.op.class(), Class::View);
    assert!(!request.op.journaled());
}
