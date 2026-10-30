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

fn device_key() -> Keypair {
    Keypair::from_secret(seq(0x01))
}

fn gateway_key() -> Keypair {
    Keypair::from_secret(seq(0x41))
}

const DEVICE_EPHEMERAL: u8 = 0x81;
const GATEWAY_EPHEMERAL: u8 = 0xc1;
const PSK: u8 = 0x21;
const RELAY_TOKEN: u8 = 0xe1;

fn vector_hello() -> DeviceHello {
    DeviceHello {
        name: "Test Phone".into(),
        platform: "ios".into(),
        ts: 1_790_000_000_000,
        api: 1,
        features: Vec::new(),
    }
}

fn vector_welcome(pairing: bool) -> Welcome {
    Welcome {
        device: "device-1".into(),
        gateway: "Desk".into(),
        token: pairing.then_some(seq(RELAY_TOKEN)),
    }
}

fn mode_name(mode: Mode<'_>) -> &'static str {
    if mode.is_pairing() {
        "pairing"
    } else {
        "reconnect"
    }
}

/// The vector handshake of `mode`: the device's message 1.
fn vector_msg1(mode: Mode<'_>) -> (Initiator, Vec<u8>) {
    Initiator::start_with(
        mode,
        &device_key(),
        gateway_key().public(),
        &vector_hello(),
        Some(&seq(DEVICE_EPHEMERAL)),
    )
    .unwrap()
}

/// Fresh (device, gateway) sessions of the vector handshake of `mode`, default config.
fn vector_sessions(mode: Mode<'_>) -> (Session, Session) {
    let (initiator, msg1) = vector_msg1(mode);
    let pending = respond_with(mode, &gateway_key(), &msg1, Some(&seq(GATEWAY_EPHEMERAL))).unwrap();
    let (gateway, msg2) = pending.accept(&vector_welcome(mode.is_pairing())).unwrap();
    let (device, _) = initiator.finish(&msg2).unwrap();
    (device, gateway)
}

/// The vector name of an error (`snake_case` variant).
fn error_name(error: &NoiseError) -> &'static str {
    match error {
        NoiseError::Handshake => "handshake",
        NoiseError::UnsupportedVersion => "unsupported_version",
        NoiseError::Payload(_) => "payload",
        NoiseError::Decrypt => "decrypt",
        NoiseError::BadFlags(_) => "bad_flags",
        NoiseError::FrameSize(_) => "frame_size",
        NoiseError::TooLarge { .. } => "too_large",
        other => panic!("no vector name for {other:?}"),
    }
}

/// A raw snow responder: the same handshake without this crate's key and payload checks.
fn snow_accepts_msg1(mode: Mode<'_>, msg1: &[u8]) -> bool {
    let psk = match mode {
        Mode::Pairing { psk } => Some(psk),
        Mode::Reconnect => None,
    };
    let gateway = gateway_key();
    let mut builder = snow::Builder::new(forge::pattern(psk).parse().unwrap())
        .prologue(PROLOGUE)
        .unwrap()
        .local_private_key(gateway.secret_bytes())
        .unwrap();
    if let Some(psk) = psk {
        builder = builder.psk(1, psk).unwrap();
    }
    let mut payload = vec![0; MAX_MESSAGE];
    builder
        .build_responder()
        .unwrap()
        .read_message(msg1, &mut payload)
        .is_ok()
}

/// Handshake messages each side must refuse, checked against this implementation.
fn rejected_handshakes() -> Vec<Value> {
    let psk = seq(PSK);
    let wrong_psk = seq(PSK + 1);
    let gateway = gateway_key();
    let device = device_key();
    let e_secret = seq(DEVICE_EPHEMERAL);
    let e_public = MontgomeryPoint::mul_base_clamped(e_secret).to_bytes();
    let es = forge::dh(&e_secret, gateway.public());
    let ss = forge::dh(device.secret_bytes(), gateway.public());
    let hello = vector_hello().encode().unwrap();
    let modes = [Mode::Pairing { psk: &psk }, Mode::Reconnect];
    let forge_psk = |mode: Mode<'_>| mode.is_pairing().then_some(&psk);

    let mut out = Vec::new();
    let mut gateway_refuses = |case: String, mode: Mode<'_>, msg1: Vec<u8>, expected: &str| {
        let error =
            respond_with(mode, &gateway, &msg1, Some(&seq(GATEWAY_EPHEMERAL))).expect_err(&case);
        assert_eq!(error_name(&error), expected, "{case}");
        out.push(json!({
            "case": case,
            "receiver": "gateway",
            "mode": mode_name(mode),
            "msg1": hex(&msg1),
            "error": expected,
        }));
    };
    let (_, msg1) = vector_msg1(Mode::Pairing { psk: &wrong_psk });
    gateway_refuses(
        "pairing message 1 with a wrong PSK".into(),
        modes[0],
        msg1,
        "handshake",
    );
    let (_, msg1) = vector_msg1(Mode::Reconnect);
    gateway_refuses(
        "reconnect message 1 to a pairing responder".into(),
        modes[0],
        msg1,
        "handshake",
    );
    for mode in modes {
        let name = mode_name(mode);
        let (_, msg1) = vector_msg1(mode);
        for (what, index) in [("static key", 40), ("payload tag", msg1.len() - 1)] {
            let mut tampered = msg1.clone();
            tampered[index] ^= 0x01;
            gateway_refuses(
                format!("{name} message 1 with a tampered {what}"),
                mode,
                tampered,
                "handshake",
            );
        }
        for point in forge::low_order_points() {
            let forged = forge::forge_msg1(
                forge_psk(mode),
                gateway.public(),
                &point,
                &[0; 32],
                device.public(),
                &ss,
                &hello,
            );
            assert!(snow_accepts_msg1(mode, &forged));
            gateway_refuses(
                format!("{name} message 1 with low-order ephemeral {}", hex(&point)),
                mode,
                forged,
                "handshake",
            );
        }
        for point in forge::low_order_points() {
            let forged = forge::forge_msg1(
                forge_psk(mode),
                gateway.public(),
                &e_public,
                &es,
                &point,
                &[0; 32],
                &hello,
            );
            assert!(snow_accepts_msg1(mode, &forged));
            gateway_refuses(
                format!("{name} message 1 with low-order static {}", hex(&point)),
                mode,
                forged,
                "handshake",
            );
        }
        let honest = |payload: &[u8]| {
            forge::forge_msg1(
                forge_psk(mode),
                gateway.public(),
                &e_public,
                &es,
                device.public(),
                &ss,
                payload,
            )
        };
        let v2 = br#"{"v":2,"name":"Test Phone","platform":"ios","ts":1790000000000}"#;
        gateway_refuses(
            format!("{name} message 1 with payload version 2"),
            mode,
            honest(v2),
            "unsupported_version",
        );
        let unsafe_name =
            "{\"v\":1,\"name\":\"Test\u{202e}Phone\",\"platform\":\"ios\",\"ts\":1790000000000}";
        gateway_refuses(
            format!("{name} message 1 with a right-to-left override in the name"),
            mode,
            honest(unsafe_name.as_bytes()),
            "payload",
        );
    }

    let mut device_refuses = |case: String, mode: Mode<'_>, msg2: Vec<u8>, expected: &str| {
        let (initiator, _) = vector_msg1(mode);
        let error = initiator.finish(&msg2).expect_err(&case);
        assert_eq!(error_name(&error), expected, "{case}");
        out.push(json!({
            "case": case,
            "receiver": "device",
            "mode": mode_name(mode),
            "msg2": hex(&msg2),
            "error": expected,
        }));
    };
    let gateway_e_secret = seq(GATEWAY_EPHEMERAL);
    let gateway_e = MontgomeryPoint::mul_base_clamped(gateway_e_secret).to_bytes();
    for mode in modes {
        let name = mode_name(mode);
        let (_, msg1) = vector_msg1(mode);
        let welcome = vector_welcome(mode.is_pairing()).encode().unwrap();
        let honest = |payload: &[u8]| {
            forge::forge_msg2(
                forge_psk(mode),
                &gateway,
                &msg1,
                &gateway_e,
                Some(&gateway_e_secret),
                payload,
            )
        };
        let mut tampered = honest(&welcome);
        *tampered.last_mut().unwrap() ^= 0x01;
        device_refuses(
            format!("{name} message 2 with a tampered tag"),
            mode,
            tampered,
            "handshake",
        );
        for point in forge::low_order_points() {
            let forged =
                forge::forge_msg2(forge_psk(mode), &gateway, &msg1, &point, None, &welcome);
            device_refuses(
                format!("{name} message 2 with low-order ephemeral {}", hex(&point)),
                mode,
                forged,
                "handshake",
            );
        }
        let v2 = br#"{"v":2,"device":"device-1","gateway":"Desk"}"#;
        device_refuses(
            format!("{name} message 2 with payload version 2"),
            mode,
            honest(v2),
            "unsupported_version",
        );
    }
    for mode in modes {
        for point in forge::low_order_points() {
            let error =
                Initiator::start_with(mode, &device, &point, &vector_hello(), Some(&e_secret))
                    .expect_err("low-order gateway key");
            assert_eq!(error_name(&error), "handshake");
            out.push(json!({
                "case": format!("{} invite with low-order gateway key {}", mode_name(mode), hex(&point)),
                "receiver": "device",
                "mode": mode_name(mode),
                "gateway_public": hex(&point),
                "error": "handshake",
            }));
        }
    }
    out
}

/// One frame sealed with arbitrary plaintext (flags byte included), as `seal` never would.
fn raw_frame(session: &mut Session, plain: &[u8]) -> Vec<u8> {
    let mut frame = vec![0u8; plain.len() + TAG_LEN];
    let len = session.transport.write_message(plain, &mut frame).unwrap();
    frame.truncate(len);
    frame
}

/// (case, receiver, reassembly cap override, frames)
type FrameCase = (&'static str, Side, Option<usize>, Vec<Vec<u8>>);

/// Transport frames a fresh reconnect session must refuse: every frame but the last opens, the
/// last fails with `error` and poisons the session.
fn rejected_frames() -> Vec<Value> {
    let psk = seq(PSK);
    let reconnect = || vector_sessions(Mode::Reconnect);
    let seal = |session: &mut Session, msg: &[u8]| session.seal(msg).unwrap();
    let mut cases: Vec<FrameCase> = Vec::new();

    let (mut device, _) = reconnect();
    let frame = seal(&mut device, b"ping").remove(0);
    for (case, edit) in [
        ("tampered ciphertext byte", 1usize),
        ("tampered tag", frame.len() - 1),
    ] {
        let mut tampered = frame.clone();
        tampered[edit] ^= 0x01;
        cases.push((case, Side::Gateway, None, vec![tampered]));
    }
    cases.push((
        "truncated frame",
        Side::Gateway,
        None,
        vec![frame[..frame.len() - 1].to_vec()],
    ));
    cases.push((
        "replayed frame",
        Side::Gateway,
        None,
        vec![frame.clone(), frame.clone()],
    ));
    let second = seal(&mut device, b"two").remove(0);
    cases.push((
        "second frame first (reordered)",
        Side::Gateway,
        None,
        vec![second],
    ));
    let (_, mut gateway) = reconnect();
    let frame = seal(&mut gateway, b"pong").remove(0);
    cases.push((
        "replayed frame to the device",
        Side::Device,
        None,
        vec![frame.clone(), frame],
    ));

    let (mut device, mut gateway) = reconnect();
    cases.push((
        "device frame reflected to the device",
        Side::Device,
        None,
        seal(&mut device, b"ping"),
    ));
    cases.push((
        "gateway frame reflected to the gateway",
        Side::Gateway,
        None,
        seal(&mut gateway, b"pong"),
    ));
    let (mut paired, _) = vector_sessions(Mode::Pairing { psk: &psk });
    cases.push((
        "frame of another session",
        Side::Gateway,
        None,
        seal(&mut paired, b"ping"),
    ));

    for (case, flags) in [
        ("unknown flag bit 0x04", 0x04u8),
        ("unknown flag bit 0x80", 0x80),
        ("unknown flag bits with FLAG_MORE", 0xfd),
    ] {
        let (mut device, _) = reconnect();
        cases.push((
            case,
            Side::Gateway,
            None,
            vec![raw_frame(&mut device, &[flags, b'x'])],
        ));
    }
    let (mut device, _) = reconnect();
    let mut short = vec![FLAG_MORE];
    short.extend_from_slice(&[0x55; 10]);
    cases.push((
        "short non-final fragment",
        Side::Gateway,
        None,
        vec![raw_frame(&mut device, &short)],
    ));
    let (mut device, _) = reconnect();
    cases.push((
        "empty non-final fragment",
        Side::Gateway,
        None,
        vec![raw_frame(&mut device, &[FLAG_MORE])],
    ));
    cases.push(("empty frame", Side::Gateway, None, vec![Vec::new()]));
    cases.push((
        "frame of only a tag",
        Side::Gateway,
        None,
        vec![vec![0xa5; TAG_LEN]],
    ));
    let (mut device, _) = reconnect();
    cases.push((
        "message over the cap",
        Side::Gateway,
        Some(4),
        seal(&mut device, b"12345"),
    ));
    let (mut device, _) = reconnect();
    cases.push((
        "reassembly over the cap",
        Side::Gateway,
        Some(MAX_FRAGMENT_DATA),
        seal(&mut device, &generated(MAX_FRAGMENT_DATA + 1)),
    ));
    let (mut device, _) = reconnect();
    let flagged = raw_frame(&mut device, &[FLAG_REKEY, b'a']);
    let stale = raw_frame(&mut device, &[0, b'b']);
    cases.push((
        "sender skipped the flagged rekey",
        Side::Gateway,
        None,
        vec![flagged, stale],
    ));

    cases
        .into_iter()
        .map(|(case, receiver, max_message, frames)| {
            let (mut device, mut gateway) = reconnect();
            let session = match receiver {
                Side::Device => &mut device,
                Side::Gateway => &mut gateway,
            };
            if let Some(max_message) = max_message {
                session.set_config(SessionConfig {
                    max_message,
                    ..SessionConfig::default()
                });
            }
            let (last, opened) = frames.split_last().unwrap();
            for frame in opened {
                session
                    .open(frame)
                    .unwrap_or_else(|e| panic!("{case}: {e:?}"));
            }
            let error = session.open(last).expect_err(case);
            assert!(matches!(
                session.open(&frames[0]),
                Err(NoiseError::Poisoned)
            ));
            json!({
                "case": case,
                "receiver": match receiver { Side::Device => "device", Side::Gateway => "gateway" },
                "max_message": max_message,
                "frames": frames.iter().map(|f| hex(f)).collect::<Vec<_>>(),
                "error": error_name(&error),
            })
        })
        .collect()
}

fn generate() -> Value {
    let device = device_key();
    let gateway = gateway_key();
    let device_ephemeral = seq(DEVICE_EPHEMERAL);
    let gateway_ephemeral = seq(GATEWAY_EPHEMERAL);
    let psk = seq(PSK);
    let hello = vector_hello();
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
        let welcome = vector_welcome(mode.is_pairing());
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
            both sessions use rekey_messages = 3. Frames above 4096 bytes are given by len + sha256 only. \
            `rejected_handshakes`: with the keys and ephemerals above, the gateway refuses `msg1`, or the device \
            (after sending the vector msg1 of `mode`) refuses `msg2`, or refuses to start against `gateway_public`, \
            with `error`. `rejected_frames`: a fresh reconnect session (`max_message` overriding the reassembly cap) \
            of `receiver` opens every frame but the last, which fails with `error` and poisons the session.",
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
        "hello": {
            "name": hello.name,
            "platform": hello.platform,
            "ts": hello.ts,
            "api": hello.api,
            "features": hello.features,
        },
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
        "rejected_handshakes": rejected_handshakes(),
        "rejected_frames": rejected_frames(),
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
