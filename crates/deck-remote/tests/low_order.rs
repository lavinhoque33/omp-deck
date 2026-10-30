#![cfg(feature = "noise")]
//! Low-order (small-subgroup) X25519 keys are refused on both sides. snow cannot emit such keys,
//! so a minimal hand-written Noise peer (`src/noise/forge.rs`, shared with the vector generator)
//! forges handshakes whose DH results are the all-zero constants a low-order key produces —
//! exactly what an attacker would send.

use deck_remote::noise::*;

#[path = "../src/noise/forge.rs"]
mod forge;

use forge::{dh, forge_msg1, forge_msg2, low_order_points, pattern};

const PSK: Psk = [7; 32];
const ZERO: [u8; 32] = [0; 32];

fn mode(psk: Option<&Psk>) -> Mode<'_> {
    match psk {
        Some(psk) => Mode::Pairing { psk },
        None => Mode::Reconnect,
    }
}

fn hello() -> DeviceHello {
    DeviceHello {
        name: "forger".into(),
        platform: "x".into(),
        ts: 1,
        api: 1,
        features: Vec::new(),
    }
}

fn welcome_payload(psk: Option<&Psk>) -> Vec<u8> {
    Welcome {
        device: "dev".into(),
        gateway: "gw".into(),
        token: psk.map(|_| [1; 32]),
    }
    .encode()
    .unwrap()
}

fn honest_keys() -> ([u8; 32], [u8; 32]) {
    let key = Keypair::generate().unwrap();
    (*key.secret_bytes(), *key.public())
}

/// A raw snow responder: the same handshake without this crate's key checks.
fn snow_accepts_msg1(psk: Option<&Psk>, gateway: &Keypair, msg1: &[u8]) -> bool {
    let mut builder = snow::Builder::new(pattern(psk).parse().unwrap())
        .prologue(PROLOGUE)
        .unwrap()
        .local_private_key(gateway.secret_bytes())
        .unwrap();
    if let Some(psk) = psk {
        builder = builder.psk(1, psk).unwrap();
    }
    let mut state = builder.build_responder().unwrap();
    let mut payload = vec![0; MAX_MESSAGE];
    state.read_message(msg1, &mut payload).is_ok()
}

#[test]
fn low_order_points_are_detected() {
    for point in low_order_points() {
        assert!(is_low_order(&point), "{point:02x?}");
    }
    for _ in 0..1000 {
        assert!(!is_low_order(Keypair::generate().unwrap().public()));
    }
}

#[test]
fn initiator_refuses_a_low_order_gateway_key() {
    let device = Keypair::generate().unwrap();
    for point in low_order_points() {
        for psk in [Some(&PSK), None] {
            let err = Initiator::start(mode(psk), &device, &point, &hello()).unwrap_err();
            assert!(matches!(err, NoiseError::Handshake), "{err:?}");
        }
    }
}

#[test]
fn forger_is_a_correct_noise_peer() {
    let gateway = Keypair::generate().unwrap();
    for psk in [Some(&PSK), None] {
        let (e_secret, e) = honest_keys();
        let (s_secret, s) = honest_keys();
        let msg1 = forge_msg1(
            psk,
            gateway.public(),
            &e,
            &dh(&e_secret, gateway.public()),
            &s,
            &dh(&s_secret, gateway.public()),
            &hello().encode().unwrap(),
        );
        let pending = respond(mode(psk), &gateway, &msg1).unwrap();
        assert_eq!(pending.device_public(), &s);
        assert_eq!(pending.hello(), &hello());
    }
}

#[test]
fn responder_refuses_a_low_order_device_ephemeral() {
    let gateway = Keypair::generate().unwrap();
    for point in low_order_points() {
        for psk in [Some(&PSK), None] {
            let (s_secret, s) = honest_keys();
            let msg1 = forge_msg1(
                psk,
                gateway.public(),
                &point,
                &ZERO,
                &s,
                &dh(&s_secret, gateway.public()),
                &hello().encode().unwrap(),
            );
            // Without the check the forged message is a valid handshake message.
            assert!(snow_accepts_msg1(psk, &gateway, &msg1));
            let err = respond(mode(psk), &gateway, &msg1).unwrap_err();
            assert!(matches!(err, NoiseError::Handshake), "{err:?}");
        }
    }
}

#[test]
fn responder_refuses_a_low_order_device_static() {
    let gateway = Keypair::generate().unwrap();
    for point in low_order_points() {
        for psk in [Some(&PSK), None] {
            let (e_secret, e) = honest_keys();
            let msg1 = forge_msg1(
                psk,
                gateway.public(),
                &e,
                &dh(&e_secret, gateway.public()),
                &point,
                &ZERO,
                &hello().encode().unwrap(),
            );
            assert!(snow_accepts_msg1(psk, &gateway, &msg1));
            let err = respond(mode(psk), &gateway, &msg1).unwrap_err();
            assert!(matches!(err, NoiseError::Handshake), "{err:?}");
        }
    }
}

fn snow_initiator(psk: Option<&Psk>, device: &Keypair, gateway: &Keypair) -> snow::HandshakeState {
    let mut builder = snow::Builder::new(pattern(psk).parse().unwrap())
        .prologue(PROLOGUE)
        .unwrap()
        .local_private_key(device.secret_bytes())
        .unwrap()
        .remote_public_key(gateway.public())
        .unwrap();
    if let Some(psk) = psk {
        builder = builder.psk(1, psk).unwrap();
    }
    builder.build_initiator().unwrap()
}

#[test]
fn initiator_refuses_a_low_order_gateway_ephemeral() {
    let gateway = Keypair::generate().unwrap();
    let device = Keypair::generate().unwrap();
    for psk in [Some(&PSK), None] {
        // Sanity: an honest forged answer completes the handshake.
        let (initiator, msg1) =
            Initiator::start(mode(psk), &device, gateway.public(), &hello()).unwrap();
        let (e_secret, e) = honest_keys();
        let msg2 = forge_msg2(
            psk,
            &gateway,
            &msg1,
            &e,
            Some(&e_secret),
            &welcome_payload(psk),
        );
        initiator.finish(&msg2).unwrap();

        for point in low_order_points() {
            let (initiator, msg1) =
                Initiator::start(mode(psk), &device, gateway.public(), &hello()).unwrap();
            let msg2 = forge_msg2(psk, &gateway, &msg1, &point, None, &welcome_payload(psk));
            let err = initiator.finish(&msg2).unwrap_err();
            assert!(matches!(err, NoiseError::Handshake), "{err:?}");

            // Without the check, snow completes this handshake with known keys.
            let mut raw = snow_initiator(psk, &device, &gateway);
            let mut buf = vec![0; MAX_MESSAGE];
            let len = raw
                .write_message(&hello().encode().unwrap(), &mut buf)
                .unwrap();
            let msg2 = forge_msg2(
                psk,
                &gateway,
                &buf[..len],
                &point,
                None,
                &welcome_payload(psk),
            );
            assert!(raw.read_message(&msg2, &mut buf).is_ok());
        }
    }
}
