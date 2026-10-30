// Record codec and ack policy (the shared Rust vectors are in api-vectors.test.ts).

import { describe, expect, test } from 'bun:test';

import { concat, toHex, utf8 } from '../src/remote/bytes.ts';
import {
  ACK_EVERY_BYTES,
  AckPolicy,
  decodeRecords,
  encodeRecords,
  MAX_APP_MESSAGE,
  MAX_INPUT,
  MAX_SNAPSHOT_CHUNK,
  type AppRecord,
  RecordError,
  type RecordErrorCode,
} from '../src/remote/records.ts';
import { CLOSE } from '../src/remote/relay.ts';

function expectCode(code: RecordErrorCode, run: () => unknown): void {
  let caught: unknown;
  try {
    run();
  } catch (error) {
    caught = error;
  }
  expect(caught).toBeInstanceOf(RecordError);
  expect(caught instanceof RecordError ? caught.code : undefined).toBe(code);
}

describe('records', () => {
  test('device records: exact bytes and round trip', () => {
    const records: AppRecord[] = [
      { kind: 'request', body: utf8('{"id":1}') },
      { kind: 'ack', messages: 0x0102030405 },
      { kind: 'input', view: 7, ctrlC: true, bytes: utf8('ab') },
      { kind: 'resize', view: 7, cols: 120, rows: 40 },
    ];
    const message = encodeRecords(records);
    expect(toHex(message)).toBe(
      `0100000008${toHex(utf8('{"id":1}'))}` +
        '02000000080000000102030405' +
        '030000000700000007016162' +
        '04000000080000000700780028',
    );
    expect(decodeRecords(message, 'device')).toEqual(records);
    expectCode('wrong_direction', () => decodeRecords(message, 'gateway'));
  });

  test('gateway records round trip; unknown kinds are skipped only from a gateway', () => {
    const records: AppRecord[] = [
      { kind: 'welcome', body: utf8('{}') },
      { kind: 't_snapshot', view: 3, last: true, bytes: new Uint8Array(MAX_SNAPSHOT_CHUNK) },
      { kind: 't_output', view: 3, bytes: utf8('x') },
      { kind: 't_resize', view: 3, cols: 80, rows: 24 },
      { kind: 't_end', view: 3, end: utf8('{"reason":"closed"}') },
    ];
    const message = encodeRecords(records);
    expect(decodeRecords(message, 'gateway')).toEqual(records);
    expectCode('wrong_direction', () => decodeRecords(message, 'device'));
    const unknown = Uint8Array.of(0x7f, 0, 0, 0, 1, 9);
    expect(decodeRecords(concat(unknown, message), 'gateway')).toEqual(records);
    // An unknown gateway-range kind is the gateway's; a device sending it is in the wrong.
    expectCode('wrong_direction', () => decodeRecords(unknown, 'device'));
    expectCode('unknown_kind', () => decodeRecords(Uint8Array.of(0x05, 0, 0, 0, 0), 'device'));
    expectCode('wrong_direction', () => decodeRecords(Uint8Array.of(0x0f, 0, 0, 0, 0), 'gateway'));
    for (const origin of ['device', 'gateway'] as const) {
      expectCode('unknown_kind', () => decodeRecords(Uint8Array.of(0x00, 0, 0, 0, 0), origin));
      expectCode('unknown_kind', () => decodeRecords(Uint8Array.of(0x80, 0, 0, 0, 0), origin));
    }
  });

  test('framing errors', () => {
    expectCode('empty', () => decodeRecords(new Uint8Array(0), 'gateway'));
    expectCode('empty', () => encodeRecords([]));
    expectCode('too_large', () => decodeRecords(new Uint8Array(MAX_APP_MESSAGE + 1), 'gateway'));
    expectCode('truncated', () => decodeRecords(Uint8Array.of(0x11, 0, 0), 'gateway'));
    expectCode('truncated', () => decodeRecords(Uint8Array.of(0x11, 0xff, 0xff, 0xff, 0xff, 0), 'gateway'));
    const four = Array.from({ length: 4 }, (): AppRecord => ({ kind: 't_output', view: 1, bytes: new Uint8Array(300_000) }));
    expectCode('too_large', () => encodeRecords(four));
    const full: AppRecord = { kind: 'reply', body: new Uint8Array(MAX_APP_MESSAGE - 5) };
    expect(encodeRecords([full]).length).toBe(MAX_APP_MESSAGE);
  });

  test('record bounds', () => {
    expectCode('oversized', () => encodeRecords([{ kind: 'input', view: 1, ctrlC: false, bytes: new Uint8Array(MAX_INPUT + 1) }]));
    expectCode('oversized', () =>
      encodeRecords([{ kind: 't_snapshot', view: 1, last: false, bytes: new Uint8Array(MAX_SNAPSHOT_CHUNK + 1) }]),
    );
    expectCode('malformed', () => encodeRecords([{ kind: 'resize', view: 1, cols: 65_536, rows: 1 }]));
    expectCode('malformed', () => encodeRecords([{ kind: 'input', view: -1, ctrlC: false, bytes: new Uint8Array(0) }]));
    expectCode('malformed', () => encodeRecords([{ kind: 'ack', messages: 2 ** 53 }]));
    expectCode('malformed', () => decodeRecords(Uint8Array.of(2, 0, 0, 0, 8, 0x80, 0, 0, 0, 0, 0, 0, 0), 'device'));
    expectCode('malformed', () => decodeRecords(Uint8Array.of(0x23, 0, 0, 0, 3, 0, 0, 1), 'gateway'));
  });

  test('close codes for protocol errors and stalls', () => {
    expect(CLOSE.APP_PROTOCOL).toBe(4002);
    expect(CLOSE.STALLED).toBe(4010);
  });
});

describe('ack policy', () => {
  test('acks every 8 messages', () => {
    const policy = new AckPolicy();
    const acks: [number, number][] = [];
    for (let i = 0; i < 17; i++) {
      const ack = policy.onMessage(10, 0);
      if (ack !== null) acks.push([i, ack]);
    }
    expect(acks).toEqual([
      [7, 8],
      [15, 16],
    ]);
    expect(policy.processed).toBe(17);
    expect(policy.flush()).toBe(17);
    expect(policy.flush()).toBeNull();
  });

  test('acks at 256 KiB', () => {
    const policy = new AckPolicy();
    expect(policy.onMessage(ACK_EVERY_BYTES - 1, 0)).toBeNull();
    expect(policy.onMessage(1, 0)).toBe(2);
    expect(policy.onMessage(ACK_EVERY_BYTES, 0)).toBe(3);
  });

  test('acks 100 ms after the first unacknowledged message', () => {
    const policy = new AckPolicy();
    expect(policy.deadline()).toBeNull();
    expect(policy.due(1000)).toBeNull();
    expect(policy.onMessage(1, 1000)).toBeNull();
    expect(policy.onMessage(1, 1050)).toBeNull();
    expect(policy.deadline()).toBe(1100);
    expect(policy.due(1099)).toBeNull();
    expect(policy.due(1100)).toBe(2);
    expect(policy.deadline()).toBeNull();
    // The timer starts at the first unacknowledged message, not at the previous ack.
    expect(policy.onMessage(1, 5000)).toBeNull();
    expect(policy.onMessage(1, 5100)).toBe(4);
  });
});
