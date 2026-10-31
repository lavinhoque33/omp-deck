# Protocol test vectors

Generated files are rewritten by their Rust test (`OMPDECK_WRITE_VECTORS=1`) and fail CI on any drift.
Hand-maintained files carry their SHA-256 below; `tests/payload.rs` and the PWA tests check it, so an edit must
update this README in the same change.

## `noise-v1.json` (OmpDeck, generated)

Deterministic vectors of the OmpDeck device protocol (`deck_remote::noise`), generated from Rust with fixed static
keys, fixed ephemerals, a fixed PSK and fixed payloads. The Rust test `noise::vectors::shared_vectors_match` regenerates
them and fails on any drift; the PWA tests (`pwa/test/`) reproduce the same bytes in TypeScript.

Both sides also run the shared rejections:

- `rejected_handshakes`: a wrong PSK, a reconnect message 1 to a pairing responder, tampered messages, low-order
  ephemeral and static keys of every encoding class (forged by the hand-written peer in `src/noise/forge.rs`; the
  generator checks that a raw `snow` responder accepts each forged message 1, so only the key check refuses it),
  payload version 2 (`unsupported_version`) and an unsafe device name (`payload`).
- `rejected_frames`: tampered, truncated, replayed, reordered and reflected frames, a frame of another session,
  unknown flag bits, short and empty non-final fragments, frames too short to hold a tag, a message and a reassembly
  over the cap, and a sender that skipped a flagged rekey.

Rewrite after an intentional protocol change:

```sh
OMPDECK_WRITE_VECTORS=1 cargo test -p deck-remote --lib noise::vectors
```

## `payload-v1.json` (OmpDeck, hand-maintained)

`payload-v1.json` SHA-256: `fe1dd690b5aae846d4fd591ef280b3c439ac9895071d69eb856cddf617f732aa`

Accept/reject vectors of the handshake payload grammar (`DeviceHello` in `hello`, `Welcome` in `welcome`): one flat
JSON object, string or integer values only (bare digits up to 2^53 − 1), no duplicate keys, valid UTF-8 without BOM,
no lone surrogate escapes, character limits in Unicode scalar values. Payloads are hex bytes; accepted entries carry
the decoded fields; rejected entries fail with `error` (`unsupported_version` for an integer `v` other than 1, else a
payload error). `tests/payload.rs` and `pwa/test/payload.test.ts` both run every entry. Edit by hand.

## `hello-v1.json` (OmpDeck, hand-maintained)

`hello-v1.json` SHA-256: `013bb93428a3344ee1909c98b921fee83aae4dc117fa6a56c7dd9587f7025566`

`display_safe`: `noise::is_display_safe` (PWA `isDisplaySafe`) on every listed code point, its neighbours and ordinary
text. `hello`: message-1 payload texts covering display-safe names and platforms, `api` and `features`, and payload
versions; an accepted one decodes to `expect` and re-encodes to exactly `encoded`. `tests/payload.rs` and
`pwa/test/payload.test.ts` run every entry. Edit by hand.

## `invite-v1.json` (OmpDeck, hand-maintained)

`invite-v1.json` SHA-256: `9ff8b33ca0d83b6ac976aefc8dfb3d6507685d5134c5563e6109c7237568eb11`

The canonical relay URL of a pairing invite (`urls`: each with its `v1.` invite over `fields`) and whole invite texts
(`invites`: byte order mark, invalid UTF-8, prefix, base64url and length rejects). `tests/invite.rs` and
`pwa/test/invite.test.ts` run every entry. Edit by hand.

## `relay-v1.json` (OmpDeck, generated)

The relay wire format (`deck_remote::relay`): both hellos, `HELLO_OK` and its acceptance rule, every `ToRelay` and
`FromRelay` message (with an empty and a full token set and the largest payload), decoder rejects by error name, the
close-code table, the room key and the token hash, and the byte layouts (`layouts`: peer ids, token counts and close
codes are little-endian, the gateway epoch is big-endian). `tests/relay_vectors.rs` regenerates and checks it through
the public API only; the PWA tests check the device-side parts.

```sh
OMPDECK_WRITE_VECTORS=1 cargo test -p deck-remote --test relay_vectors
```

## `api-v1.json` (OmpDeck, generated)

The device application protocol (`deck_remote::api`): `records` (whole application messages as hex, with their records;
`origin` is the sender), `skipped_records` (messages a device decodes to the listed records, skipping the unknown
gateway-range kinds between them), `rejected_records` (messages refused whole, by error name; long ones are `hex`
followed by `pad.count` copies of `pad.byte`), `messages` (the canonical JSON text of every request, reply, event,
welcome and `T_END` variant: decoding gives the value, encoding gives the exact text; `None` is `null`) and
`rejected_requests` (`REQUEST` bodies answered `unsupported` — an unknown op, feed, action type or enum value — or
`bad_request` by id, or closing the session as `unreadable`). `tests/api.rs` regenerates and checks them, and asserts
per api enum (an exhaustive `match` per type) that every variant appears; the PWA tests read the same file.

```sh
OMPDECK_WRITE_VECTORS=1 cargo test -p deck-remote --test api
```

`api-v1.json` is the frozen R5 baseline: `tests/api.rs` also asserts it regenerates byte-identically, so additive
slice-4 fields (omitted when empty) cannot change it.

## `api-s4.json` (OmpDeck, generated)

The slice-4 additions (`docs/notes/mobile-pwa.md`, "Wire additions"), used only when both sides announce the
`features` listed in the file (`"s4"`): `messages` (the canonical JSON text of every new op, feed, feed value, reply,
outcome result and enum value, and of the additive fields of R5 types — `Welcome.push_key`, `SessionSummary.stub`,
`LiveSession.pending_ui`, `images`/`args_value`/`details` of transcript items — which, unlike every other `None`, are
omitted when empty) and `rejected_requests` (new request bodies answered `unsupported` or `bad_request` by id).
Records and framing are unchanged and stay in `api-v1.json`. `tests/api_s4/mod.rs` (part of the `api` test) writes and
checks it; the variant-coverage check spans both files. Regenerated by the same command as `api-v1.json`; the PWA tests
read it too.

## `cacophony-ik.json` (official, copied)

`cacophony-ik.json` SHA-256: `95f24bddaabfce40922ebe6f3a06e7b2a1b0e9cdd3c6c23b87654046ac529175`

The `Noise_IK_25519_ChaChaPoly_BLAKE2s` and `Noise_IKpsk1_25519_ChaChaPoly_BLAKE2s` entries, unchanged, from
`tests/vectors/cacophony.txt` of the `snow` 0.10.0 crate (crates.io; file SHA-256
`3bde7c09a6f349ee11c825c50fcc02649f8f02a47c857a459206b357f9386cae`). That file is the test-vector output of
cacophony, the Haskell Noise implementation (<https://github.com/haskell-cryptography/cacophony>), whose package
declares `license: PublicDomain`. Format: the Noise wiki test-vector JSON (`protocol_name`, `init_*`/`resp_*` keys,
`messages[]` of `payload`/`ciphertext` alternating initiator → responder, `handshake_hash`).
