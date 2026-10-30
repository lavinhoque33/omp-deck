# Noise test vectors

## `noise-v1.json` (OmpDeck, generated)

Deterministic vectors of the OmpDeck device protocol (`deck_remote::noise`), generated from Rust with fixed static
keys, fixed ephemerals, a fixed PSK and fixed payloads. The Rust test `noise::vectors::shared_vectors_match` regenerates
them and fails on any drift; the PWA tests (`pwa/test/`) reproduce the same bytes in TypeScript.

Rewrite after an intentional protocol change:

```sh
OMPDECK_WRITE_VECTORS=1 cargo test -p deck-remote --lib noise::vectors
```

## `payload-v1.json` (OmpDeck, hand-maintained)

Accept/reject vectors of the handshake payload grammar (`DeviceHello` in `hello`, `Welcome` in `welcome`): one flat
JSON object, string or integer values only (bare digits up to 2^53 − 1), no duplicate keys, valid UTF-8 without BOM,
no lone surrogate escapes, character limits in Unicode scalar values. Payloads are hex bytes; accepted entries carry
the decoded fields. `tests/payload.rs` and `pwa/test/payload.test.ts` both run every entry. Edit by hand.

## `api-v1.json` (OmpDeck, generated)

The device application protocol (`deck_remote::api`): `records` (whole application messages as hex, with their records;
`origin` is the sender), `rejected_records` (messages refused whole, by error name; long ones are `hex` followed by
`pad.count` copies of `pad.byte`), `messages` (the canonical JSON text of every request, reply, event, welcome and
`T_END` variant: decoding gives the value, encoding gives the exact text; `None` is `null`) and `rejected_requests`
(`REQUEST` bodies answered `unsupported`/`bad_request` by id, or closing the session as `unreadable`).
`tests/api.rs` regenerates and checks them; the PWA tests read the same file.

```sh
OMPDECK_WRITE_VECTORS=1 cargo test -p deck-remote --test api
```

## `cacophony-ik.json` (official, copied)

The `Noise_IK_25519_ChaChaPoly_BLAKE2s` and `Noise_IKpsk1_25519_ChaChaPoly_BLAKE2s` entries, unchanged, from
`tests/vectors/cacophony.txt` of the `snow` 0.10.0 crate (crates.io; file SHA-256
`3bde7c09a6f349ee11c825c50fcc02649f8f02a47c857a459206b357f9386cae`). That file is the test-vector output of
cacophony, the Haskell Noise implementation (<https://github.com/haskell-cryptography/cacophony>), whose package
declares `license: PublicDomain`. Format: the Noise wiki test-vector JSON (`protocol_name`, `init_*`/`resp_*` keys,
`messages[]` of `payload`/`ciphertext` alternating initiator → responder, `handshake_hash`).
