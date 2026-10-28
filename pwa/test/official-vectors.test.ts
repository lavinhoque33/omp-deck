// The official cacophony vectors for Noise_IK and Noise_IKpsk1 (25519, ChaChaPoly, BLAKE2s);
// see crates/deck-remote/tests/vectors/README.md for their source.

import { expect, test } from 'bun:test';

import { fromHex, toHex } from '../src/remote/bytes.ts';
import { type CipherState, HandshakeState, type Pattern, PROTOCOL_NAMES, publicKeyOf } from '../src/remote/noise.ts';
import { officialVectors } from './vectors.ts';

const vectors = officialVectors();

test('both patterns are covered', () => {
  expect(vectors.map((vector) => vector.protocol_name)).toEqual([PROTOCOL_NAMES.IK, PROTOCOL_NAMES.IKpsk1]);
});

function keypair(secretHex: string): { secret: Uint8Array; public: Uint8Array } {
  const secret = fromHex(secretHex);
  return { secret, public: publicKeyOf(secret) };
}

for (const vector of vectors) {
  test(vector.protocol_name, () => {
    const pattern: Pattern = vector.protocol_name === PROTOCOL_NAMES.IK ? 'IK' : 'IKpsk1';
    const initPsk = vector.init_psks?.[0];
    const respPsk = vector.resp_psks?.[0];
    const initiator = new HandshakeState({
      pattern,
      initiator: true,
      prologue: fromHex(vector.init_prologue),
      s: keypair(vector.init_static),
      rs: fromHex(vector.init_remote_static),
      ephemeral: fromHex(vector.init_ephemeral),
      ...(initPsk === undefined ? {} : { psk: fromHex(initPsk) }),
    });
    const responder = new HandshakeState({
      pattern,
      initiator: false,
      prologue: fromHex(vector.resp_prologue),
      s: keypair(vector.resp_static),
      ephemeral: fromHex(vector.resp_ephemeral),
      ...(respPsk === undefined ? {} : { psk: fromHex(respPsk) }),
    });

    const [first, second, ...transport] = vector.messages;
    for (const [message, writer, reader] of [
      [first!, initiator, responder],
      [second!, responder, initiator],
    ] as const) {
      const ciphertext = writer.writeMessage(fromHex(message.payload));
      expect(toHex(ciphertext)).toBe(message.ciphertext);
      expect(toHex(reader.readMessage(ciphertext))).toBe(message.payload);
    }
    expect(initiator.finished && responder.finished).toBe(true);
    expect(toHex(initiator.handshakeHash)).toBe(vector.handshake_hash);
    expect(toHex(responder.handshakeHash)).toBe(vector.handshake_hash);

    const [initSend, initReceive] = initiator.split();
    const [respSend, respReceive] = responder.split();
    transport.forEach((message, i) => {
      const [send, receive]: [CipherState, CipherState] =
        i % 2 === 0 ? [initSend, respReceive] : [respSend, initReceive];
      const ciphertext = send.encryptWithAd(new Uint8Array(0), fromHex(message.payload));
      expect(toHex(ciphertext)).toBe(message.ciphertext);
      expect(toHex(receive.decryptWithAd(new Uint8Array(0), ciphertext))).toBe(message.payload);
    });
  });
}
