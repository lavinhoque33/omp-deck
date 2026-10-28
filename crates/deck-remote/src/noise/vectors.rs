//! Deterministic protocol vectors shared with the PWA (`tests/vectors/noise-v1.json`), and the
//! official cacophony vectors for the two Noise patterns (`tests/vectors/cacophony-ik.json`).
//!
//! Rewrite the shared file after an intentional protocol change:
//! `OMPDECK_WRITE_VECTORS=1 cargo test -p deck-remote --lib noise::vectors`.

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::*;

const SHARED: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/vectors/noise-v1.json");
const OFFICIAL: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/vectors/cacophony-ik.json"
);
/// Frames up to this size are stored as hex; larger ones by length and SHA-256.
const INLINE_FRAME: usize = 4096;
const REKEY_MESSAGES: u64 = 3;

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(text: &str) -> Vec<u8> {
    assert!(text.len().is_multiple_of(2), "odd hex length");
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).expect("hex"))
        .collect()
}

fn seq(start: u8) -> [u8; 32] {
    std::array::from_fn(|i| start.wrapping_add(i as u8))
}

/// Byte `i` of a generated plaintext: `(7 * i + 3) mod 256`.
fn generated(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i * 7 + 3) as u8).collect()
}

enum Side {
    Device,
    Gateway,
}

enum Plain {
    Bytes(&'static [u8]),
    Generated(usize),
}

const STEPS: &[(Side, Plain)] = &[
    (Side::Device, Plain::Bytes(b"ping")),
    // Two fragments; the second is the device's third frame and carries REKEY.
    (Side::Device, Plain::Generated(MAX_FRAGMENT_DATA + 100)),
    (Side::Device, Plain::Bytes(b"after rekey")),
    (Side::Gateway, Plain::Bytes(b"pong")),
    // Exactly one full fragment.
    (Side::Gateway, Plain::Generated(MAX_FRAGMENT_DATA)),
    // The gateway's third frame: an empty message carrying REKEY.
    (Side::Gateway, Plain::Bytes(b"")),
    (Side::Gateway, Plain::Bytes(b"after rekey")),
    // Three fragments; the second is the device's third frame since its rekey: MORE | REKEY.
    (Side::Device, Plain::Generated(2 * MAX_FRAGMENT_DATA + 1)),
];

fn frame_json(frame: &[u8], flags: u8) -> Value {
    let mut out = json!({
        "flags": flags,
        "len": frame.len(),
        "sha256": hex(&Sha256::digest(frame)),
    });
    if frame.len() <= INLINE_FRAME {
        out["hex"] = json!(hex(frame));
    }
    out
}

fn generate() -> Value {
    let device = Keypair::from_secret(seq(0x01));
    let gateway = Keypair::from_secret(seq(0x41));
    let device_ephemeral = seq(0x81);
    let gateway_ephemeral = seq(0xc1);
    let psk = seq(0x21);
    let relay_token = seq(0xe1);
    let hello = DeviceHello {
        name: "Test Phone".into(),
        platform: "ios".into(),
        ts: 1_790_000_000_000,
    };
    let invite = PairingInvite {
        url: "wss://relay.example.test".into(),
        room: std::array::from_fn(|i| 0x61 + i as u8),
        token: seq(0xa1),
        gateway_public: *gateway.public(),
        psk: Zeroizing::new(psk),
    };

    let mut handshakes = Vec::new();
    for (mode, name) in [
        (Mode::Pairing { psk: &psk }, "pairing"),
        (Mode::Reconnect, "reconnect"),
    ] {
        let welcome = Welcome {
            device: "device-1".into(),
            gateway: "Desk".into(),
            token: mode.is_pairing().then_some(relay_token),
        };
        let (initiator, msg1) = Initiator::start_with(
            mode,
            &device,
            gateway.public(),
            &hello,
            Some(&device_ephemeral),
        )
        .unwrap();
        let pending = respond_with(mode, &gateway, &msg1, Some(&gateway_ephemeral)).unwrap();
        assert_eq!(pending.device_public(), device.public());
        assert_eq!(pending.hello(), &hello);
        let (mut gateway_session, msg2) = pending.accept(&welcome).unwrap();
        let (mut device_session, received) = initiator.finish(&msg2).unwrap();
        assert_eq!(received, welcome);
        assert_eq!(
            device_session.handshake_hash(),
            gateway_session.handshake_hash()
        );
        let config = SessionConfig {
            rekey_messages: REKEY_MESSAGES,
            ..SessionConfig::default()
        };
        device_session.set_config(config);
        gateway_session.set_config(config);

        let mut sent = [0u64; 2];
        let mut transport = Vec::new();
        for (from, plain) in STEPS {
            let (sender, receiver, side, label) = match from {
                Side::Device => (&mut device_session, &mut gateway_session, 0, "device"),
                Side::Gateway => (&mut gateway_session, &mut device_session, 1, "gateway"),
            };
            let (bytes, plain_json) = match plain {
                Plain::Bytes(bytes) => (bytes.to_vec(), json!({ "plaintext_hex": hex(bytes) })),
                Plain::Generated(len) => (generated(*len), json!({ "plaintext_generated": len })),
            };
            let frames = sender.seal(&bytes).unwrap();
            let mut frames_json = Vec::new();
            for (index, frame) in frames.iter().enumerate() {
                sent[side] += 1;
                let mut flags = if index + 1 < frames.len() {
                    FLAG_MORE
                } else {
                    0
                };
                if sent[side] % REKEY_MESSAGES == 0 {
                    flags |= FLAG_REKEY;
                }
                frames_json.push(frame_json(frame, flags));
                let opened = receiver.open(frame).unwrap();
                if index + 1 < frames.len() {
                    assert_eq!(opened, None);
                } else {
                    assert_eq!(opened.as_deref(), Some(bytes.as_slice()));
                }
            }
            let mut step = plain_json;
            step["from"] = json!(label);
            step["frames"] = json!(frames_json);
            transport.push(step);
        }

        handshakes.push(json!({
            "mode": name,
            "pattern": if mode.is_pairing() { PAIRING_PATTERN } else { RECONNECT_PATTERN },
            "welcome": {
                "device": welcome.device,
                "gateway": welcome.gateway,
                "token": welcome.token.map(|t| URL_SAFE_NO_PAD.encode(t)),
            },
            "welcome_payload": hex(&welcome.encode().unwrap()),
            "msg1": hex(&msg1),
            "msg2": hex(&msg2),
            "handshake_hash": hex(device_session.handshake_hash()),
            "transport": transport,
        }));
    }

    json!({
        "description": "OmpDeck device protocol v1 vectors, generated by deck_remote::noise (see README.md). \
            Device = initiator, gateway = responder. Transport frames alternate per the `from` side; \
            both sessions use rekey_messages = 3. Frames above 4096 bytes are given by len + sha256 only.",
        "prologue": hex(PROLOGUE),
        "plaintext_generator": "byte i = (7 * i + 3) mod 256",
        "max_fragment_data": MAX_FRAGMENT_DATA,
        "rekey_messages": REKEY_MESSAGES,
        "device_static": hex(device.secret_bytes()),
        "device_public": hex(device.public()),
        "device_fingerprint": fingerprint(device.public()),
        "device_ephemeral": hex(&device_ephemeral),
        "gateway_static": hex(gateway.secret_bytes()),
        "gateway_public": hex(gateway.public()),
        "gateway_fingerprint": fingerprint(gateway.public()),
        "gateway_ephemeral": hex(&gateway_ephemeral),
        "psk": hex(&psk),
        "hello": { "name": hello.name, "platform": hello.platform, "ts": hello.ts },
        "hello_payload": hex(&hello.encode().unwrap()),
        "invite": {
            "url": invite.url,
            "room": hex(&invite.room),
            "token": hex(&invite.token),
            "gateway_public": hex(&invite.gateway_public),
            "psk": hex(invite.psk.as_ref()),
            "encoded": invite.encode().unwrap(),
        },
        // The device's first relay message when pairing with this invite.
        "relay_hello": hex(&crate::relay::Hello::Device { room: invite.room, token: invite.token }.encode()),
        "handshakes": handshakes,
    })
}

#[test]
fn shared_vectors_match() {
    let fresh = generate();
    if std::env::var_os("OMPDECK_WRITE_VECTORS").is_some_and(|v| v == "1") {
        let mut text = serde_json::to_string_pretty(&fresh).unwrap();
        text.push('\n');
        std::fs::write(SHARED, text).unwrap();
        return;
    }
    let stored: Value = serde_json::from_str(
        &std::fs::read_to_string(SHARED)
            .expect("missing vectors; run with OMPDECK_WRITE_VECTORS=1"),
    )
    .unwrap();
    assert!(
        stored == fresh,
        "noise-v1.json drifted from the implementation; if intended, rewrite it with \
         OMPDECK_WRITE_VECTORS=1 cargo test -p deck-remote --lib noise::vectors"
    );
}

#[test]
fn generation_is_deterministic() {
    assert_eq!(generate(), generate());
}

fn field(vector: &Value, key: &str) -> Vec<u8> {
    unhex(
        vector[key]
            .as_str()
            .unwrap_or_else(|| panic!("missing {key}")),
    )
}

#[test]
fn official_cacophony_vectors() {
    let file: Value = serde_json::from_str(&std::fs::read_to_string(OFFICIAL).unwrap()).unwrap();
    let vectors = file["vectors"].as_array().unwrap();
    let names: Vec<&str> = vectors
        .iter()
        .map(|v| v["protocol_name"].as_str().unwrap())
        .collect();
    assert_eq!(names, [RECONNECT_PATTERN, PAIRING_PATTERN]);

    for vector in vectors {
        let params: NoiseParams = vector["protocol_name"].as_str().unwrap().parse().unwrap();
        let (init_s, init_e, init_rs) = (
            field(vector, "init_static"),
            field(vector, "init_ephemeral"),
            field(vector, "init_remote_static"),
        );
        let (resp_s, resp_e) = (
            field(vector, "resp_static"),
            field(vector, "resp_ephemeral"),
        );
        let (init_p, resp_p) = (
            field(vector, "init_prologue"),
            field(vector, "resp_prologue"),
        );
        let psks = |key: &str| -> Option<Psk> {
            vector[key]
                .as_array()
                .map(|list| unhex(list[0].as_str().unwrap()).try_into().unwrap())
        };
        let (init_psk, resp_psk) = (psks("init_psks"), psks("resp_psks"));

        let mut init = snow::Builder::new(params.clone())
            .prologue(&init_p)
            .unwrap()
            .local_private_key(&init_s)
            .unwrap()
            .remote_public_key(&init_rs)
            .unwrap()
            .fixed_ephemeral_key_for_testing_only(&init_e);
        let mut resp = snow::Builder::new(params)
            .prologue(&resp_p)
            .unwrap()
            .local_private_key(&resp_s)
            .unwrap()
            .fixed_ephemeral_key_for_testing_only(&resp_e);
        if let (Some(a), Some(b)) = (&init_psk, &resp_psk) {
            init = init.psk(1, a).unwrap();
            resp = resp.psk(1, b).unwrap();
        }
        let mut init = init.build_initiator().unwrap();
        let mut resp = resp.build_responder().unwrap();

        let messages = vector["messages"].as_array().unwrap();
        let mut buf = vec![0u8; MAX_MESSAGE];
        let mut out = vec![0u8; MAX_MESSAGE];
        for message in &messages[..2] {
            let payload = field(message, "payload");
            let expected = field(message, "ciphertext");
            let (writer, reader) = if init.is_my_turn() {
                (&mut init, &mut resp)
            } else {
                (&mut resp, &mut init)
            };
            let len = writer.write_message(&payload, &mut buf).unwrap();
            assert_eq!(hex(&buf[..len]), hex(&expected));
            let read = reader.read_message(&expected, &mut out).unwrap();
            assert_eq!(&out[..read], payload.as_slice());
        }
        assert_eq!(
            hex(init.get_handshake_hash()),
            vector["handshake_hash"].as_str().unwrap()
        );
        let mut init = init.into_transport_mode().unwrap();
        let mut resp = resp.into_transport_mode().unwrap();
        for (index, message) in messages[2..].iter().enumerate() {
            let payload = field(message, "payload");
            let expected = field(message, "ciphertext");
            let (writer, reader) = if index % 2 == 0 {
                (&mut init, &mut resp)
            } else {
                (&mut resp, &mut init)
            };
            let len = writer.write_message(&payload, &mut buf).unwrap();
            assert_eq!(hex(&buf[..len]), hex(&expected));
            let read = reader.read_message(&expected, &mut out).unwrap();
            assert_eq!(&out[..read], payload.as_slice());
        }
    }
}
