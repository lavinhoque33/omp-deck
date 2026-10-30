//! Byte vectors of the relay wire format (`tests/vectors/relay-v1.json`), generated and checked
//! through `deck_remote::relay`'s public API only. A codec change that is symmetric (encode and
//! decode changed together) still fails here; the PWA tests read the device-side parts.
//!
//! Rewrite after an intentional protocol change:
//! `OMPDECK_WRITE_VECTORS=1 cargo test -p deck-remote --test relay_vectors`

use deck_remote::relay::*;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/vectors/relay-v1.json");
/// Messages up to this size are stored as hex; larger ones by length and SHA-256.
const INLINE: usize = 4096;

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(text: &str) -> Vec<u8> {
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).expect("hex"))
        .collect()
}

fn seq<const N: usize>(start: u8) -> [u8; N] {
    std::array::from_fn(|i| start.wrapping_add(i as u8))
}

/// Byte `j` of generated hash `i`: `(i + 7 * j) mod 256`.
fn hashes(count: usize) -> Vec<Hash> {
    (0..count)
        .map(|i| std::array::from_fn(|j| (i + 7 * j) as u8))
        .collect()
}

/// Byte `i` of a generated payload: `(7 * i + 3) mod 256`.
fn payload(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i * 7 + 3) as u8).collect()
}

/// `hex`, or `len` + `sha256` above [`INLINE`] bytes.
fn bytes_json(bytes: &[u8]) -> Value {
    if bytes.len() <= INLINE {
        json!({ "hex": hex(bytes) })
    } else {
        json!({ "len": bytes.len(), "sha256": hex(&Sha256::digest(bytes)) })
    }
}

fn error_name(error: &WireError) -> &'static str {
    match error {
        WireError::Empty => "empty",
        WireError::UnknownType(_) => "unknown_type",
        WireError::Version(_) => "version",
        WireError::Malformed(_) => "malformed",
        WireError::TooLarge(_) => "too_large",
        WireError::TooManyTokens(_) => "too_many_tokens",
        WireError::RoomKey => "room_key",
    }
}

const CLOSE_CODES: &[(&str, u16)] = &[
    ("NORMAL", close::NORMAL),
    ("GOING_AWAY", close::GOING_AWAY),
    ("PROTOCOL", close::PROTOCOL),
    ("BAD_HELLO", close::BAD_HELLO),
    ("APP_PROTOCOL", close::APP_PROTOCOL),
    ("UNAUTHORIZED", close::UNAUTHORIZED),
    ("GATEWAY_OFFLINE", close::GATEWAY_OFFLINE),
    ("SLOW_CONSUMER", close::SLOW_CONSUMER),
    ("REPLACED", close::REPLACED),
    ("STALLED", close::STALLED),
    ("TOO_LARGE", close::TOO_LARGE),
    ("UPGRADE_REQUIRED", close::UPGRADE_REQUIRED),
    ("RATE_LIMITED", close::RATE_LIMITED),
];

fn room() -> Room {
    seq(0x61)
}

fn hellos() -> Vec<(&'static str, Hello, Value)> {
    let token: Token = seq(0xa1);
    let secret: Secret = seq(0x11);
    vec![
        (
            "device",
            Hello::Device {
                room: room(),
                token,
            },
            json!({ "role": "device", "room": hex(&room()), "token": hex(&token) }),
        ),
        (
            "gateway",
            Hello::Gateway {
                room: room(),
                secret,
                epoch: 0x0102_0304_0506_0708,
            },
            json!({ "role": "gateway", "room": hex(&room()), "secret": hex(&secret), "epoch": 0x0102_0304_0506_0708_u64 }),
        ),
        (
            "gateway at epoch 0",
            Hello::Gateway {
                room: room(),
                secret,
                epoch: 0,
            },
            json!({ "role": "gateway", "room": hex(&room()), "secret": hex(&secret), "epoch": 0 }),
        ),
    ]
}

fn to_relay(storage: &[Vec<u8>]) -> Vec<(String, ToRelay<'_>, Value)> {
    let [small, max] = storage else {
        unreachable!()
    };
    let mut out = vec![
        (
            "data".to_owned(),
            ToRelay::Data {
                peer: 0x0403_0201,
                payload: small,
            },
            json!({ "type": "data", "peer": 0x0403_0201_u32, "payload": hex(small) }),
        ),
        (
            "data with an empty payload".to_owned(),
            ToRelay::Data {
                peer: u32::MAX,
                payload: &[],
            },
            json!({ "type": "data", "peer": u32::MAX, "payload": "" }),
        ),
        (
            "data with the largest payload".to_owned(),
            ToRelay::Data {
                peer: 7,
                payload: max,
            },
            json!({ "type": "data", "peer": 7, "payload_generated": max.len() }),
        ),
        (
            "close".to_owned(),
            ToRelay::Close {
                peer: 0x0403_0201,
                code: close::UNAUTHORIZED,
            },
            json!({ "type": "close", "peer": 0x0403_0201_u32, "code": close::UNAUTHORIZED }),
        ),
    ];
    for count in [0, 1, 3, MAX_TOKENS] {
        out.push((
            format!("tokens ({count})"),
            ToRelay::Tokens(hashes(count)),
            json!({ "type": "tokens", "hashes_generated": count }),
        ));
    }
    out
}

fn from_relay(storage: &[Vec<u8>]) -> Vec<(&'static str, FromRelay<'_>, Value)> {
    let [small, max] = storage else {
        unreachable!()
    };
    let token = hash(&seq(0xa1));
    vec![
        (
            "data",
            FromRelay::Data {
                peer: 0x0403_0201,
                payload: small,
            },
            json!({ "type": "data", "peer": 0x0403_0201_u32, "payload": hex(small) }),
        ),
        (
            "data with the largest payload",
            FromRelay::Data {
                peer: 7,
                payload: max,
            },
            json!({ "type": "data", "peer": 7, "payload_generated": max.len() }),
        ),
        (
            "open",
            FromRelay::Open {
                peer: 0x0403_0201,
                token,
            },
            json!({ "type": "open", "peer": 0x0403_0201_u32, "token": hex(&token) }),
        ),
        (
            "closed",
            FromRelay::Closed { peer: 0x0403_0201 },
            json!({ "type": "closed", "peer": 0x0403_0201_u32 }),
        ),
    ]
}

/// (case, decoder, bytes, padding (byte, count) appended to the bytes)
type Reject = (&'static str, &'static str, Vec<u8>, Option<(u8, usize)>);

fn rejected() -> Vec<Reject> {
    let device = hellos()[0].1.encode();
    let gateway = hellos()[1].1.encode();
    let with = |base: &[u8], extra: &[u8]| [base, extra].concat();
    let mut wrong_version = device.clone();
    wrong_version[1] = 2;
    let mut gateway_version_0 = gateway.clone();
    gateway_version_0[1] = 0;
    let mut gateway_without_epoch = device.clone();
    gateway_without_epoch[0] = b'G';
    let mut device_with_epoch = gateway.clone();
    device_with_epoch[0] = b'D';
    let peer = [1, 2, 3, 4];
    vec![
        ("empty", "hello", vec![], None),
        ("unknown hello tag", "hello", with(b"X", &device[1..]), None),
        (
            "lowercase device tag",
            "hello",
            with(b"d", &device[1..]),
            None,
        ),
        ("device hello of version 2", "hello", wrong_version, None),
        (
            "gateway hello of version 0",
            "hello",
            gateway_version_0,
            None,
        ),
        ("only a tag", "hello", vec![b'D'], None),
        (
            "device hello one byte short",
            "hello",
            device[..device.len() - 1].to_vec(),
            None,
        ),
        (
            "device hello with a trailing byte",
            "hello",
            with(&device, &[0]),
            None,
        ),
        (
            "device hello carrying an epoch",
            "hello",
            device_with_epoch,
            None,
        ),
        (
            "gateway hello without its epoch",
            "hello",
            gateway_without_epoch,
            None,
        ),
        (
            "gateway hello with a trailing byte",
            "hello",
            with(&gateway, &[0]),
            None,
        ),
        ("empty", "to_relay", vec![], None),
        (
            "unknown type 0x00",
            "to_relay",
            vec![0x00, 1, 2, 3, 4],
            None,
        ),
        (
            "unknown type 0x04",
            "to_relay",
            vec![0x04, 1, 2, 3, 4],
            None,
        ),
        (
            "a relay-to-gateway type",
            "to_relay",
            with(&[0x81], &peer),
            None,
        ),
        (
            "data without a full peer id",
            "to_relay",
            vec![0x01, 1, 2, 3],
            None,
        ),
        (
            "data over the payload cap",
            "to_relay",
            with(&[0x01], &peer),
            Some((0xab, MAX_PAYLOAD + 1)),
        ),
        (
            "tokens without a full count",
            "to_relay",
            vec![0x02, 1],
            None,
        ),
        ("tokens missing a hash", "to_relay", vec![0x02, 1, 0], None),
        (
            "tokens with a trailing byte",
            "to_relay",
            with(&[0x02, 1, 0], &[0x55; HASH_LEN + 1]),
            None,
        ),
        (
            "tokens count over the cap",
            "to_relay",
            vec![0x02, 0x01, 0x04],
            None,
        ),
        (
            "close without a code",
            "to_relay",
            with(&[0x03], &peer),
            None,
        ),
        (
            "close with half a code",
            "to_relay",
            with(&[0x03], &[1, 2, 3, 4, 0xa3]),
            None,
        ),
        (
            "close with a trailing byte",
            "to_relay",
            with(&[0x03], &[1, 2, 3, 4, 0xa3, 0x0f, 0]),
            None,
        ),
        ("empty", "from_relay", vec![], None),
        (
            "unknown type 0x84",
            "from_relay",
            with(&[0x84], &peer),
            None,
        ),
        (
            "a gateway-to-relay type",
            "from_relay",
            with(&[0x01], &peer),
            None,
        ),
        (
            "data without a full peer id",
            "from_relay",
            vec![0x81, 1, 2],
            None,
        ),
        (
            "data over the payload cap",
            "from_relay",
            with(&[0x81], &peer),
            Some((0xab, MAX_PAYLOAD + 1)),
        ),
        (
            "open with a short hash",
            "from_relay",
            with(&[0x82], &[1, 2, 3, 4, 9, 9]),
            None,
        ),
        (
            "open with a trailing byte",
            "from_relay",
            with(&with(&[0x82], &peer), &[0x55; HASH_LEN + 1]),
            None,
        ),
        (
            "closed without a full peer id",
            "from_relay",
            vec![0x83, 1, 2, 3],
            None,
        ),
        (
            "closed with a trailing byte",
            "from_relay",
            with(&[0x83], &[1, 2, 3, 4, 0]),
            None,
        ),
    ]
}

fn decode_error(decoder: &str, bytes: &[u8]) -> WireError {
    match decoder {
        "hello" => Hello::decode(bytes).expect_err("hello accepted"),
        "to_relay" => ToRelay::decode(bytes).expect_err("to_relay accepted"),
        "from_relay" => FromRelay::decode(bytes).expect_err("from_relay accepted"),
        other => panic!("unknown decoder {other}"),
    }
}

fn build() -> Value {
    let storage = vec![b"noise".to_vec(), payload(MAX_PAYLOAD)];
    let hellos: Vec<Value> = hellos()
        .into_iter()
        .map(|(case, hello, fields)| {
            let bytes = hello.encode();
            assert_eq!(Hello::decode(&bytes).unwrap(), hello, "{case}");
            let mut out = json!({ "case": case, "hex": hex(&bytes) });
            out["fields"] = fields;
            out
        })
        .collect();
    let to_relay: Vec<Value> = to_relay(&storage)
        .into_iter()
        .map(|(case, message, fields)| {
            let bytes = message.encode().unwrap();
            assert_eq!(ToRelay::decode(&bytes).unwrap(), message, "{case}");
            let mut out = bytes_json(&bytes);
            out["case"] = json!(case);
            out["fields"] = fields;
            out
        })
        .collect();
    let from_relay: Vec<Value> = from_relay(&storage)
        .into_iter()
        .map(|(case, message, fields)| {
            let bytes = message.encode().unwrap();
            assert_eq!(FromRelay::decode(&bytes).unwrap(), message, "{case}");
            let mut out = bytes_json(&bytes);
            out["case"] = json!(case);
            out["fields"] = fields;
            out
        })
        .collect();
    let rejected: Vec<Value> = rejected()
        .into_iter()
        .map(|(case, decoder, mut bytes, pad)| {
            let mut out = json!({ "case": case, "decoder": decoder, "hex": hex(&bytes) });
            if let Some((byte, count)) = pad {
                bytes.extend(std::iter::repeat_n(byte, count));
                out["pad"] = json!({ "byte": byte, "count": count });
            }
            out["error"] = json!(error_name(&decode_error(decoder, &bytes)));
            out
        })
        .collect();
    let room_key = RoomKey {
        room: room(),
        secret: seq(0x11),
    };
    let token: Token = seq(0xa1);
    let close_codes: serde_json::Map<String, Value> = CLOSE_CODES
        .iter()
        .map(|(name, code)| ((*name).to_owned(), json!(code)))
        .collect();
    json!({
        "description": "Relay protocol v1 wire vectors, generated by crates/deck-remote/tests/relay_vectors.rs from \
            deck_remote::relay (see README.md). Messages above 4096 bytes are given by len + sha256. Generated \
            payloads: byte i = (7 * i + 3) mod 256; generated token hashes: byte j of hash i = (i + 7 * j) mod 256. \
            `rejected`: `decoder` refuses `hex` (followed by `pad.count` copies of `pad.byte`) with `error`. \
            A client accepts any HELLO_OK whose first byte is 0 (`hello_ok_accepted`/`hello_ok_rejected`).",
        "version": VERSION,
        "paths": { "gateway": GATEWAY_PATH, "device": DEVICE_PATH, "health": HEALTH_PATH },
        "max_payload": MAX_PAYLOAD,
        "max_gateway_message": MAX_GATEWAY_MESSAGE,
        "max_tokens": MAX_TOKENS,
        "layouts": {
            "hello_device": "'D' u8 | version u8 | room [16] | admission token [32]",
            "hello_gateway": "'G' u8 | version u8 | room [16] | room secret [32] | epoch u64 big-endian",
            "hello_ok": "0x00, then reserved bytes",
            "to_relay_data": "0x01 | peer u32 little-endian | payload (≤ max_payload)",
            "to_relay_tokens": "0x02 | count u16 little-endian (≤ max_tokens) | count × sha256 hash [32]",
            "to_relay_close": "0x03 | peer u32 little-endian | close code u16 little-endian",
            "from_relay_data": "0x81 | peer u32 little-endian | payload (≤ max_payload)",
            "from_relay_open": "0x82 | peer u32 little-endian | token sha256 hash [32]",
            "from_relay_closed": "0x83 | peer u32 little-endian",
            "device_payload": "one Noise message (≤ max_payload), no header",
            "room_key": "\"ompdeck-relay-v1:\" + base64url without padding (room [16] | secret [32])",
        },
        "hello_ok": hex(&HELLO_OK),
        "hello_ok_accepted": ["00", "0001", "00ff00"],
        "hello_ok_rejected": ["", "01", "ff00"],
        "close_codes": close_codes,
        "room_key": {
            "room": hex(&room_key.room),
            "secret": hex(&room_key.secret),
            "encoded": room_key.encode(),
        },
        "token_hash": { "token": hex(&token), "sha256": hex(&hash(&token)) },
        "hellos": hellos,
        "to_relay": to_relay,
        "from_relay": from_relay,
        "rejected": rejected,
    })
}

fn stored() -> Value {
    let text = std::fs::read_to_string(PATH).expect("relay-v1.json (OMPDECK_WRITE_VECTORS=1)");
    serde_json::from_str(&text).unwrap()
}

#[test]
fn shared_vectors_match() {
    let built = build();
    if std::env::var_os("OMPDECK_WRITE_VECTORS").is_some_and(|v| v == "1") {
        let text = serde_json::to_string_pretty(&built).unwrap() + "\n";
        std::fs::write(PATH, text).unwrap();
    }
    assert!(
        stored() == built,
        "relay-v1.json drifted from deck_remote::relay; if intended, rewrite it with \
         OMPDECK_WRITE_VECTORS=1 cargo test -p deck-remote --test relay_vectors"
    );
}

/// The stored bytes decode to the stored fields, independent of the generator above.
#[test]
fn stored_messages_decode_to_their_fields() {
    let file = stored();
    for vector in file["hellos"].as_array().unwrap() {
        let case = vector["case"].as_str().unwrap();
        let fields = &vector["fields"];
        let room: Room = unhex(fields["room"].as_str().unwrap()).try_into().unwrap();
        let expected = match fields["role"].as_str().unwrap() {
            "device" => Hello::Device {
                room,
                token: unhex(fields["token"].as_str().unwrap()).try_into().unwrap(),
            },
            _ => Hello::Gateway {
                room,
                secret: unhex(fields["secret"].as_str().unwrap())
                    .try_into()
                    .unwrap(),
                epoch: fields["epoch"].as_u64().unwrap(),
            },
        };
        let bytes = unhex(vector["hex"].as_str().unwrap());
        assert_eq!(Hello::decode(&bytes).unwrap(), expected, "{case}");
        assert_eq!(expected.encode(), bytes, "{case}");
    }
    for vector in file["from_relay"].as_array().unwrap() {
        let case = vector["case"].as_str().unwrap();
        let Some(text) = vector["hex"].as_str() else {
            continue;
        };
        let bytes = unhex(text);
        let fields = &vector["fields"];
        let peer = fields["peer"].as_u64().unwrap() as u32;
        let decoded = FromRelay::decode(&bytes).unwrap();
        let expected_payload = fields["payload"].as_str().map(unhex);
        let expected = match fields["type"].as_str().unwrap() {
            "data" => FromRelay::Data {
                peer,
                payload: expected_payload.as_deref().unwrap(),
            },
            "open" => FromRelay::Open {
                peer,
                token: unhex(fields["token"].as_str().unwrap()).try_into().unwrap(),
            },
            _ => FromRelay::Closed { peer },
        };
        assert_eq!(decoded, expected, "{case}");
    }
    for vector in file["rejected"].as_array().unwrap() {
        let case = vector["case"].as_str().unwrap();
        let mut bytes = unhex(vector["hex"].as_str().unwrap());
        if let Some(pad) = vector.get("pad") {
            let byte = pad["byte"].as_u64().unwrap() as u8;
            bytes.extend(std::iter::repeat_n(
                byte,
                pad["count"].as_u64().unwrap() as usize,
            ));
        }
        let error = decode_error(vector["decoder"].as_str().unwrap(), &bytes);
        assert_eq!(
            error_name(&error),
            vector["error"].as_str().unwrap(),
            "{case}"
        );
    }
    let key = &file["room_key"];
    let parsed = RoomKey::parse(key["encoded"].as_str().unwrap()).unwrap();
    assert_eq!(hex(&parsed.room), key["room"].as_str().unwrap());
    assert_eq!(hex(&parsed.secret), key["secret"].as_str().unwrap());
    assert_eq!(file["hello_ok"].as_str().unwrap(), hex(&HELLO_OK));
}
