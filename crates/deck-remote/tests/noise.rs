#![cfg(feature = "noise")]
//! Behaviour and rejection tests of `deck_remote::noise` through its public API (raw `snow` only
//! plays a misbehaving peer).

use std::time::{Duration, Instant};

use deck_remote::noise::*;

const PSK: Psk = [7; 32];
const TOKEN: [u8; 32] = [9; 32];

fn hello() -> DeviceHello {
    DeviceHello {
        name: "Pixel".into(),
        platform: "android".into(),
        ts: 1_000,
    }
}

fn welcome(pairing: bool) -> Welcome {
    Welcome {
        device: "dev-1".into(),
        gateway: "Desk".into(),
        token: pairing.then_some(TOKEN),
    }
}

fn keys() -> (Keypair, Keypair) {
    (Keypair::generate().unwrap(), Keypair::generate().unwrap())
}

/// (device session, gateway session)
fn connect(mode: Mode<'_>) -> (Session, Session) {
    let (device, gateway) = keys();
    let pairing = matches!(mode, Mode::Pairing { .. });
    let (initiator, msg1) = Initiator::start(mode, &device, gateway.public(), &hello()).unwrap();
    let pending = respond(mode, &gateway, &msg1).unwrap();
    assert_eq!(pending.device_public(), device.public());
    assert_eq!(pending.hello(), &hello());
    let (gateway_session, msg2) = pending.accept(&welcome(pairing)).unwrap();
    let (device_session, got) = initiator.finish(&msg2).unwrap();
    assert_eq!(got, welcome(pairing));
    assert_eq!(
        device_session.handshake_hash(),
        gateway_session.handshake_hash()
    );
    (device_session, gateway_session)
}

fn pair() -> (Session, Session) {
    connect(Mode::Pairing { psk: &PSK })
}

fn deliver(from: &mut Session, to: &mut Session, msg: &[u8]) {
    let frames = from.seal(msg).unwrap();
    let last = frames.len() - 1;
    for (i, frame) in frames.iter().enumerate() {
        let opened = to.open(frame).unwrap();
        if i == last {
            assert_eq!(opened.as_deref(), Some(msg));
        } else {
            assert_eq!(opened, None);
        }
    }
}

/// A device driven by raw snow, for frames the API never produces.
fn raw_device(gateway_mode_pairing: bool) -> (snow::TransportState, Session) {
    let (device, gateway) = keys();
    let pattern = if gateway_mode_pairing {
        PAIRING_PATTERN
    } else {
        RECONNECT_PATTERN
    };
    let mut builder = snow::Builder::new(pattern.parse().unwrap())
        .prologue(PROLOGUE)
        .unwrap()
        .local_private_key(device.secret_bytes())
        .unwrap()
        .remote_public_key(gateway.public())
        .unwrap();
    if gateway_mode_pairing {
        builder = builder.psk(1, &PSK).unwrap();
    }
    let mut state = builder.build_initiator().unwrap();
    let mut msg1 = vec![0; MAX_MSG1];
    let len = state
        .write_message(&hello().encode().unwrap(), &mut msg1)
        .unwrap();
    let mode = if gateway_mode_pairing {
        Mode::Pairing { psk: &PSK }
    } else {
        Mode::Reconnect
    };
    let pending = respond(mode, &gateway, &msg1[..len]).unwrap();
    let (session, msg2) = pending.accept(&welcome(gateway_mode_pairing)).unwrap();
    let mut payload = vec![0; MAX_MSG2];
    state.read_message(&msg2, &mut payload).unwrap();
    (state.into_transport_mode().unwrap(), session)
}

fn raw_frame(transport: &mut snow::TransportState, plain: &[u8]) -> Vec<u8> {
    let mut frame = vec![0; plain.len() + TAG_LEN];
    let len = transport.write_message(plain, &mut frame).unwrap();
    frame.truncate(len);
    frame
}

fn raw_open(transport: &mut snow::TransportState, frame: &[u8]) -> Result<Vec<u8>, snow::Error> {
    let mut plain = vec![0; frame.len()];
    let len = transport.read_message(frame, &mut plain)?;
    plain.truncate(len);
    Ok(plain)
}

/// A raw initiator writing an arbitrary message-1 payload.
fn raw_msg1(mode_pairing: bool, psk: &Psk, gateway_public: &PublicKey, payload: &[u8]) -> Vec<u8> {
    let device = Keypair::generate().unwrap();
    let pattern = if mode_pairing {
        PAIRING_PATTERN
    } else {
        RECONNECT_PATTERN
    };
    let mut builder = snow::Builder::new(pattern.parse().unwrap())
        .prologue(PROLOGUE)
        .unwrap()
        .local_private_key(device.secret_bytes())
        .unwrap()
        .remote_public_key(gateway_public)
        .unwrap();
    if mode_pairing {
        builder = builder.psk(1, psk).unwrap();
    }
    let mut state = builder.build_initiator().unwrap();
    let mut msg1 = vec![0; MAX_MESSAGE];
    let len = state.write_message(payload, &mut msg1).unwrap();
    msg1.truncate(len);
    msg1
}

#[test]
fn both_handshakes_complete_and_carry_traffic() {
    for (mut device, mut gateway) in [pair(), connect(Mode::Reconnect)] {
        deliver(&mut device, &mut gateway, b"hello gateway");
        deliver(&mut gateway, &mut device, b"hello device");
        deliver(&mut device, &mut gateway, b"");
    }
}

#[test]
fn sessions_have_distinct_ids() {
    let (a, _) = pair();
    let (b, _) = pair();
    assert_ne!(a.handshake_hash(), b.handshake_hash());
}

#[test]
fn wrong_psk_is_rejected_by_the_responder() {
    let (device, gateway) = keys();
    let other: Psk = [8; 32];
    let (_, msg1) = Initiator::start(
        Mode::Pairing { psk: &other },
        &device,
        gateway.public(),
        &hello(),
    )
    .unwrap();
    let err = respond(Mode::Pairing { psk: &PSK }, &gateway, &msg1).unwrap_err();
    assert!(matches!(err, NoiseError::Handshake), "{err:?}");
}

#[test]
fn pairing_message_is_not_accepted_as_reconnect_and_vice_versa() {
    let (device, gateway) = keys();
    let (_, msg1) = Initiator::start(
        Mode::Pairing { psk: &PSK },
        &device,
        gateway.public(),
        &hello(),
    )
    .unwrap();
    assert!(matches!(
        respond(Mode::Reconnect, &gateway, &msg1),
        Err(NoiseError::Handshake)
    ));
    let (_, msg1) = Initiator::start(Mode::Reconnect, &device, gateway.public(), &hello()).unwrap();
    assert!(matches!(
        respond(Mode::Pairing { psk: &PSK }, &gateway, &msg1),
        Err(NoiseError::Handshake)
    ));
}

#[test]
fn wrong_gateway_key_is_rejected() {
    let (device, gateway) = keys();
    let impostor = Keypair::generate().unwrap();
    for mode in [Mode::Pairing { psk: &PSK }, Mode::Reconnect] {
        let (_, msg1) = Initiator::start(mode, &device, impostor.public(), &hello()).unwrap();
        let err = respond(mode, &gateway, &msg1).unwrap_err();
        assert!(matches!(err, NoiseError::Handshake), "{err:?}");
    }
}

#[test]
fn reconnect_exposes_the_device_key_before_any_answer() {
    let (device, gateway) = keys();
    let (initiator, msg1) =
        Initiator::start(Mode::Reconnect, &device, gateway.public(), &hello()).unwrap();
    let pending = respond(Mode::Reconnect, &gateway, &msg1).unwrap();
    assert_eq!(pending.device_public(), device.public());
    assert_eq!(pending.hello().ts, 1_000);
    // A revoked device: the gateway drops the pending handshake; no message 2 exists.
    drop(pending);
    // A forged message 2 does not complete the device's handshake either.
    assert!(matches!(
        initiator.finish(&[0u8; 64]),
        Err(NoiseError::Handshake)
    ));
}

#[test]
fn tampered_truncated_and_oversized_handshake_messages_are_rejected() {
    let (device, gateway) = keys();
    let mode = Mode::Pairing { psk: &PSK };
    let (initiator, msg1) = Initiator::start(mode, &device, gateway.public(), &hello()).unwrap();
    for i in [0, 40, msg1.len() - 1] {
        let mut bad = msg1.clone();
        bad[i] ^= 0x01;
        assert!(matches!(
            respond(mode, &gateway, &bad),
            Err(NoiseError::Handshake)
        ));
    }
    assert!(matches!(
        respond(mode, &gateway, &msg1[..90]),
        Err(NoiseError::MessageSize(90))
    ));
    let mut long = msg1.clone();
    long.resize(MAX_MSG1 + 1, 0);
    assert!(matches!(
        respond(mode, &gateway, &long),
        Err(NoiseError::MessageSize(_))
    ));

    let (_, msg2) = respond(mode, &gateway, &msg1)
        .unwrap()
        .accept(&welcome(true))
        .unwrap();
    let mut bad = msg2.clone();
    bad[msg2.len() - 1] ^= 0x80;
    assert!(matches!(initiator.finish(&bad), Err(NoiseError::Handshake)));
}

#[test]
fn malformed_and_oversized_hello_payloads_are_rejected() {
    let gateway = Keypair::generate().unwrap();
    let big = format!(
        r#"{{"v":1,"name":"a","platform":"b","ts":1,"pad":"{}"}}"#,
        "x".repeat(1100)
    );
    let cases: [(&[u8], bool); 5] = [
        (b"not json", false),
        (br#"{"v":2,"name":"a","platform":"b","ts":1}"#, false),
        (br#"{"v":1,"name":"a","ts":1}"#, false),
        (
            br#"{"v":1,"name":"a","platform":"b","ts":9007199254740992}"#,
            false,
        ),
        (big.as_bytes(), true),
    ];
    for (payload, too_big) in cases {
        let msg1 = raw_msg1(true, &PSK, gateway.public(), payload);
        let err = respond(Mode::Pairing { psk: &PSK }, &gateway, &msg1).unwrap_err();
        if too_big {
            assert!(matches!(err, NoiseError::MessageSize(_)), "{err:?}");
        } else {
            assert!(matches!(err, NoiseError::Payload(_)), "{err:?}");
        }
    }
    // Unknown string and integer fields are ignored; any other unknown value is rejected.
    let msg1 = raw_msg1(
        false,
        &PSK,
        gateway.public(),
        br#"{"v":1,"name":"a","platform":"b","ts":5,"extra":"x","n":2}"#,
    );
    let pending = respond(Mode::Reconnect, &gateway, &msg1).unwrap();
    assert_eq!(pending.hello().ts, 5);
    let msg1 = raw_msg1(
        false,
        &PSK,
        gateway.public(),
        br#"{"v":1,"name":"a","platform":"b","ts":5,"extra":[1,2]}"#,
    );
    assert!(matches!(
        respond(Mode::Reconnect, &gateway, &msg1),
        Err(NoiseError::Payload(_))
    ));
}

#[test]
fn payload_limits() {
    let mut h = hello();
    h.name = "é".repeat(MAX_NAME_CHARS);
    assert_eq!(DeviceHello::decode(&h.encode().unwrap()).unwrap(), h);
    h.name.push('é');
    assert!(matches!(h.encode(), Err(NoiseError::Payload(_))));
    let mut h = hello();
    h.platform = "p".repeat(MAX_PLATFORM_CHARS + 1);
    assert!(matches!(h.encode(), Err(NoiseError::Payload(_))));
    let mut h = hello();
    h.ts = MAX_TIMESTAMP + 1;
    assert!(matches!(h.encode(), Err(NoiseError::Payload(_))));
    let pad = format!(
        r#"{{"v":1,"name":"a","platform":"b","ts":1,"pad":"{}"}}"#,
        "x".repeat(1000)
    );
    assert!(matches!(
        DeviceHello::decode(pad.as_bytes()),
        Err(NoiseError::Payload(_))
    ));

    let mut w = welcome(true);
    w.device = "d".repeat(MAX_DEVICE_ID_CHARS + 1);
    assert!(matches!(w.encode(), Err(NoiseError::Payload(_))));
    let mut w = welcome(true);
    w.gateway = "g".repeat(MAX_GATEWAY_LABEL_CHARS + 1);
    assert!(matches!(w.encode(), Err(NoiseError::Payload(_))));
    let w = welcome(true);
    assert_eq!(Welcome::decode(&w.encode().unwrap()).unwrap(), w);
    assert!(matches!(
        Welcome::decode(br#"{"v":1,"device":"d","gateway":"g","token":"AAAA"}"#),
        Err(NoiseError::Payload(_))
    ));
}

#[test]
fn welcome_token_must_match_the_mode() {
    let (device, gateway) = keys();
    let (_, msg1) = Initiator::start(
        Mode::Pairing { psk: &PSK },
        &device,
        gateway.public(),
        &hello(),
    )
    .unwrap();
    let pending = respond(Mode::Pairing { psk: &PSK }, &gateway, &msg1).unwrap();
    assert!(matches!(
        pending.accept(&welcome(false)),
        Err(NoiseError::Payload(_))
    ));
    let (_, msg1) = Initiator::start(Mode::Reconnect, &device, gateway.public(), &hello()).unwrap();
    let pending = respond(Mode::Reconnect, &gateway, &msg1).unwrap();
    assert!(matches!(
        pending.accept(&welcome(true)),
        Err(NoiseError::Payload(_))
    ));
}

fn assert_poisoned(session: &mut Session) {
    assert!(session.is_poisoned());
    assert!(matches!(session.open(&[0; 32]), Err(NoiseError::Poisoned)));
    assert!(matches!(session.seal(b"x"), Err(NoiseError::Poisoned)));
}

#[test]
fn tampered_frame_poisons_the_session() {
    let (mut device, mut gateway) = pair();
    let frame = device.seal(b"secret").unwrap().remove(0);
    let mut bad = frame.clone();
    bad[3] ^= 0x10;
    assert!(matches!(gateway.open(&bad), Err(NoiseError::Decrypt)));
    assert_poisoned(&mut gateway);
    assert!(matches!(gateway.open(&frame), Err(NoiseError::Poisoned)));
}

#[test]
fn replayed_frame_poisons_the_session() {
    let (mut device, mut gateway) = pair();
    let frame = device.seal(b"once").unwrap().remove(0);
    assert_eq!(gateway.open(&frame).unwrap().as_deref(), Some(&b"once"[..]));
    assert!(matches!(gateway.open(&frame), Err(NoiseError::Decrypt)));
    assert_poisoned(&mut gateway);
}

#[test]
fn reordered_frames_poison_the_session() {
    let (mut device, mut gateway) = pair();
    let first = device.seal(b"one").unwrap().remove(0);
    let second = device.seal(b"two").unwrap().remove(0);
    assert!(matches!(gateway.open(&second), Err(NoiseError::Decrypt)));
    assert_poisoned(&mut gateway);
    assert!(matches!(gateway.open(&first), Err(NoiseError::Poisoned)));
}

#[test]
fn reordered_fragments_poison_the_session() {
    let (mut device, mut gateway) = pair();
    let frames = device.seal(&vec![1u8; MAX_FRAGMENT_DATA + 1]).unwrap();
    assert_eq!(frames.len(), 2);
    assert!(matches!(gateway.open(&frames[1]), Err(NoiseError::Decrypt)));
    assert_poisoned(&mut gateway);
}

#[test]
fn unknown_flag_bits_and_missing_flags_poison_the_session() {
    for bits in [0x04u8, 0x80, 0xff] {
        let (mut raw, mut gateway) = raw_device(true);
        let frame = raw_frame(&mut raw, &[bits, b'x']);
        assert!(matches!(gateway.open(&frame), Err(NoiseError::BadFlags(b)) if b == bits));
        assert_poisoned(&mut gateway);
    }
    // A frame without a flags byte (empty plaintext).
    let (mut raw, mut gateway) = raw_device(false);
    let frame = raw_frame(&mut raw, &[]);
    assert_eq!(frame.len(), TAG_LEN);
    assert!(matches!(
        gateway.open(&frame),
        Err(NoiseError::FrameSize(16))
    ));
    assert_poisoned(&mut gateway);
}

#[test]
fn raw_peer_sees_the_wire_flags() {
    let (mut raw, mut gateway) = raw_device(true);
    let frames = gateway.seal(&vec![5u8; MAX_FRAGMENT_DATA + 1]).unwrap();
    let first = raw_open(&mut raw, &frames[0]).unwrap();
    let second = raw_open(&mut raw, &frames[1]).unwrap();
    assert_eq!(first[0], FLAG_MORE);
    assert_eq!(first.len(), 1 + MAX_FRAGMENT_DATA);
    assert_eq!(second, [0, 5]);
}

#[test]
fn fragmentation_boundary() {
    let (mut device, mut gateway) = pair();
    let exact = vec![0xabu8; MAX_FRAGMENT_DATA];
    let frames = device.seal(&exact).unwrap();
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].len(), MAX_MESSAGE);
    assert_eq!(gateway.open(&frames[0]).unwrap(), Some(exact));

    let over = vec![0xcdu8; MAX_FRAGMENT_DATA + 1];
    let frames = device.seal(&over).unwrap();
    assert_eq!(frames.len(), 2);
    assert_eq!(frames[0].len(), MAX_MESSAGE);
    assert_eq!(frames[1].len(), 1 + 1 + TAG_LEN);
    assert_eq!(gateway.open(&frames[0]).unwrap(), None);
    assert_eq!(gateway.open(&frames[1]).unwrap(), Some(over));

    let big: Vec<u8> = (0..3 * MAX_FRAGMENT_DATA + 7).map(|i| i as u8).collect();
    deliver(&mut device, &mut gateway, &big);
    deliver(&mut gateway, &mut device, &big);
}

#[test]
fn short_non_final_fragment_poisons_the_session() {
    let mut full = vec![FLAG_MORE];
    full.resize(1 + MAX_FRAGMENT_DATA, 3);
    for len in [1, MAX_FRAGMENT_DATA - 1] {
        let (mut raw, mut gateway) = raw_device(true);
        let frame = raw_frame(&mut raw, &full[..1 + len]);
        assert!(matches!(gateway.open(&frame), Err(NoiseError::FrameSize(n)) if n == frame.len()));
        assert_poisoned(&mut gateway);
    }
    // After a valid full fragment too.
    let (mut raw, mut gateway) = raw_device(true);
    assert_eq!(gateway.open(&raw_frame(&mut raw, &full)).unwrap(), None);
    let frame = raw_frame(&mut raw, &full[..100]);
    assert!(matches!(
        gateway.open(&frame),
        Err(NoiseError::FrameSize(_))
    ));
    assert_poisoned(&mut gateway);
}

#[test]
fn empty_non_final_fragment_poisons_the_session() {
    let (mut raw, mut gateway) = raw_device(false);
    let frame = raw_frame(&mut raw, &[FLAG_MORE]);
    assert!(matches!(
        gateway.open(&frame),
        Err(NoiseError::FrameSize(n)) if n == 1 + TAG_LEN
    ));
    assert_poisoned(&mut gateway);
}

#[test]
fn many_fragment_message_at_the_default_cap() {
    let (mut device, mut gateway) = pair();
    let cap = DEFAULT_MAX_REASSEMBLY;
    let msg: Vec<u8> = (0..cap).map(|i| (i % 251) as u8).collect();
    let frames = device.seal(&msg).unwrap();
    assert_eq!(frames.len(), cap.div_ceil(MAX_FRAGMENT_DATA));
    let (last, rest) = frames.split_last().unwrap();
    for frame in rest {
        assert_eq!(frame.len(), MAX_MESSAGE);
        assert_eq!(gateway.open(frame).unwrap(), None);
    }
    assert_eq!(gateway.open(last).unwrap(), Some(msg));

    // One byte more: the sender may allow it, the receiver's cap rejects it.
    device.set_config(SessionConfig {
        max_message: cap + 1,
        ..SessionConfig::default()
    });
    let frames = device.seal(&vec![0; cap + 1]).unwrap();
    let (last, rest) = frames.split_last().unwrap();
    for frame in rest {
        assert_eq!(gateway.open(frame).unwrap(), None);
    }
    assert!(matches!(
        gateway.open(last),
        Err(NoiseError::TooLarge { size, cap: c }) if size == cap + 1 && c == cap
    ));
    assert_poisoned(&mut gateway);
}

#[test]
fn oversized_frame_is_rejected() {
    let (_, mut gateway) = pair();
    assert!(matches!(
        gateway.open(&vec![0; MAX_MESSAGE + 1]),
        Err(NoiseError::FrameSize(_))
    ));
    assert_poisoned(&mut gateway);
}

#[test]
fn reassembly_cap() {
    let (mut device, mut gateway) = pair();
    let cap = 100_000;
    gateway.set_config(SessionConfig {
        max_message: cap,
        ..SessionConfig::default()
    });
    deliver(&mut device, &mut gateway, &vec![1u8; cap]);
    let frames = device.seal(&vec![2u8; cap + 1]).unwrap();
    assert_eq!(gateway.open(&frames[0]).unwrap(), None);
    assert!(matches!(
        gateway.open(&frames[1]),
        Err(NoiseError::TooLarge { size, cap: c }) if size == cap + 1 && c == cap
    ));
    assert_poisoned(&mut gateway);

    // The sender refuses messages over its own cap.
    device.set_config(SessionConfig {
        max_message: 10,
        ..SessionConfig::default()
    });
    assert!(matches!(
        device.seal(&[0; 11]),
        Err(NoiseError::TooLarge { size: 11, cap: 10 })
    ));
    assert!(device.is_poisoned());
}

#[test]
fn default_config() {
    let config = SessionConfig::default();
    assert_eq!(config.rekey_messages, 1 << 20);
    assert_eq!(config.rekey_interval, Duration::from_secs(1800));
    assert_eq!(config.max_message, 16 * 1024 * 1024);
}

#[test]
fn rekey_by_count_keeps_both_sides_in_sync() {
    let (mut device, mut gateway) = pair();
    let config = SessionConfig {
        rekey_messages: 2,
        ..SessionConfig::default()
    };
    device.set_config(config);
    gateway.set_config(config);
    for i in 0..20u32 {
        deliver(&mut device, &mut gateway, &i.to_le_bytes());
        deliver(&mut gateway, &mut device, &(i * 3).to_le_bytes());
        deliver(
            &mut device,
            &mut gateway,
            &vec![i as u8; MAX_FRAGMENT_DATA * 2 + 3],
        );
    }
}

#[test]
fn rekey_flag_really_rekeys() {
    let (mut raw, mut gateway) = raw_device(true);
    gateway.set_config(SessionConfig {
        rekey_messages: 2,
        ..SessionConfig::default()
    });
    let a = gateway.seal(b"a").unwrap().remove(0);
    let b = gateway.seal(b"b").unwrap().remove(0);
    let c = gateway.seal(b"c").unwrap().remove(0);
    assert_eq!(raw_open(&mut raw, &a).unwrap(), [0, b'a']);
    assert_eq!(raw_open(&mut raw, &b).unwrap(), [FLAG_REKEY, b'b']);
    // A peer that skips REKEY cannot read on.
    assert!(raw_open(&mut raw, &c).is_err());

    // And the gateway rekeys its receiving side at a flagged frame from the device.
    let (mut raw, mut gateway) = raw_device(false);
    let flagged = raw_frame(&mut raw, &[FLAG_REKEY, b'x']);
    assert_eq!(gateway.open(&flagged).unwrap().as_deref(), Some(&b"x"[..]));
    raw.rekey_outgoing();
    let next = raw_frame(&mut raw, &[0, b'y']);
    assert_eq!(gateway.open(&next).unwrap().as_deref(), Some(&b"y"[..]));
}

#[test]
fn rekey_by_time_keeps_both_sides_in_sync() {
    let (mut raw, mut gateway) = raw_device(true);
    let start = Instant::now();
    let early = gateway.seal_at(b"early", start).unwrap().remove(0);
    assert_eq!(raw_open(&mut raw, &early).unwrap()[0], 0);
    let later = start + DEFAULT_REKEY_INTERVAL + Duration::from_secs(1);
    let late = gateway.seal_at(b"late", later).unwrap().remove(0);
    assert_eq!(raw_open(&mut raw, &late).unwrap()[0], FLAG_REKEY);
    raw.rekey_incoming();
    // The timer restarted: the next frame is not flagged.
    let next = gateway
        .seal_at(b"next", later + Duration::from_secs(60))
        .unwrap()
        .remove(0);
    assert_eq!(raw_open(&mut raw, &next).unwrap(), b"\0next");

    let (mut device, mut gateway) = pair();
    let mut now = Instant::now();
    for i in 0..10u8 {
        now += Duration::from_secs(20 * 60);
        let frames = device.seal_at(&[i], now).unwrap();
        assert_eq!(gateway.open(&frames[0]).unwrap(), Some(vec![i]));
        let frames = gateway.seal_at(&[i, i], now).unwrap();
        assert_eq!(device.open(&frames[0]).unwrap(), Some(vec![i, i]));
    }
}

#[test]
fn keypair_round_trip_and_fingerprint() {
    let key = Keypair::generate().unwrap();
    let restored = Keypair::from_secret(*key.secret_bytes());
    assert_eq!(restored.public(), key.public());
    let fp = fingerprint(key.public());
    assert_eq!(fp.len(), 24);
    let groups: Vec<&str> = fp.split('-').collect();
    assert_eq!(groups.len(), 5);
    assert!(groups.iter().all(|g| {
        g.len() == 4
            && g.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    }));
    assert!(!format!("{key:?}").contains(&format!("{:?}", key.secret_bytes())));
}

#[test]
fn timestamps() {
    assert!(timestamp_fresh(5, 6));
    assert!(!timestamp_fresh(5, 5));
    assert!(!timestamp_fresh(5, 4));
    assert_eq!(next_timestamp(10, 5), 11);
    assert_eq!(next_timestamp(10, 50), 50);
}

fn invite() -> PairingInvite {
    PairingInvite {
        url: "wss://relay.example.org:8443/base".into(),
        room: [1; 16],
        token: [2; 32],
        gateway_public: [3; 32],
        psk: zeroize::Zeroizing::new([4; 32]),
    }
}

#[test]
fn invite_round_trip() {
    let invite = invite();
    let text = invite.encode().unwrap();
    assert!(text.starts_with(INVITE_PREFIX));
    assert_eq!(PairingInvite::parse(&text).unwrap(), invite);
    let mut ws = invite.clone();
    ws.url = format!("ws://{}", "h".repeat(MAX_INVITE_URL - 5));
    assert_eq!(ws.url.len(), MAX_INVITE_URL);
    assert_eq!(PairingInvite::parse(&ws.encode().unwrap()).unwrap(), ws);
    assert!(!format!("{invite:?}").contains("[4, 4"));
}

#[test]
fn invite_rejects() {
    use base64::Engine as _;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;

    let text = invite().encode().unwrap();
    let body = &text[INVITE_PREFIX.len()..];
    assert!(PairingInvite::parse(body).is_err());
    assert!(PairingInvite::parse(&format!("v2.{body}")).is_err());
    assert!(PairingInvite::parse("v1.!!!").is_err());
    assert!(PairingInvite::parse("v1.").is_err());

    let raw = URL_SAFE_NO_PAD.decode(body).unwrap();
    for bad in [&raw[..raw.len() - 1], &[raw.as_slice(), &[0]].concat()[..]] {
        let err = PairingInvite::parse(&format!("v1.{}", URL_SAFE_NO_PAD.encode(bad))).unwrap_err();
        assert!(matches!(err, NoiseError::Invite("wrong length")), "{err:?}");
    }

    for url in ["https://relay.example.org", "wss://", "relay.example.org"] {
        let mut bad = invite();
        bad.url = url.into();
        assert!(bad.encode().is_err(), "{url}");
        // And a parser never accepts it either.
        let mut raw = vec![url.len() as u8];
        raw.extend_from_slice(url.as_bytes());
        raw.extend_from_slice(&[0; 16 + 32 + 32 + 32]);
        assert!(PairingInvite::parse(&format!("v1.{}", URL_SAFE_NO_PAD.encode(&raw))).is_err());
    }
    let mut long = invite();
    long.url = format!("wss://{}", "h".repeat(MAX_INVITE_URL));
    assert!(matches!(long.encode(), Err(NoiseError::Invite(_))));
}
