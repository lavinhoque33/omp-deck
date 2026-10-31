// The device's connection to its gateway through the relay: one browser WebSocket carrying the
// relay hello, the Noise handshake and then sealed application messages (mobile-relay.md "Relay
// protocol", R1, R4). The client sees only records: `Link` hides every byte below them, and the
// client's tests replace it with an in-memory fake.

import {
  CLOSE,
  DEVICE_PATH,
  decodeRecords,
  encodeRecords,
  encodeRelayHello,
  Initiator,
  isHelloOk,
  Keypair,
  MAX_APP_MESSAGE,
  NoiseError,
  RecordError,
  type AppRecord,
  type DeviceHello,
  type PairingInvite,
  type Session,
} from '../remote/index.ts';

/** How far a connection got before it closed. */
export type LinkStage = 'socket' | 'admission' | 'handshake' | 'session';

export interface LinkClose {
  /** The WebSocket close code (1006 when the socket failed without one). */
  code: number;
  reason: string;
  stage: LinkStage;
  /** A local failure that made the device close (undecodable frame, handshake failure). */
  error: string | null;
}

export interface LinkEvents {
  /** The handshake completed; records may flow. */
  open(): void;
  /** One gateway application message, decoded; `size` is its plaintext length. */
  message(records: AppRecord[], size: number): void;
  /** Called exactly once, also after `close()`. */
  closed(info: LinkClose): void;
}

export interface Link {
  send(records: readonly AppRecord[]): void;
  close(code: number): void;
}

/** What a reconnect needs from the stored pairing. */
export interface DialTarget {
  url: string;
  room: Uint8Array;
  token: Uint8Array;
  secret: Uint8Array;
  gatewayPublic: Uint8Array;
  hello: DeviceHello;
}

export type Dialer = (target: DialTarget, events: LinkEvents) => Link;

/** The WebSocket constructor seam (a browser's by default). */
export type SocketFactory = (url: string) => WebSocket;

const browserSocket: SocketFactory = (url) => new WebSocket(url);

/** A reconnect handshake that has not finished after this long is abandoned. */
const HANDSHAKE_TIMEOUT_MS = 20_000;

function describe(error: unknown): string {
  if (error instanceof NoiseError || error instanceof RecordError) return `${error.code}: ${error.message}`;
  return error instanceof Error ? error.message : String(error);
}

/** Opens a reconnect (IK) session to the gateway through the relay. */
export function dialRelay(target: DialTarget, events: LinkEvents, socketFactory: SocketFactory = browserSocket): Link {
  const socket = socketFactory(`${target.url}${DEVICE_PATH}`);
  socket.binaryType = 'arraybuffer';
  let stage: LinkStage = 'socket';
  let initiator: Initiator | null = null;
  let session: Session | null = null;
  let error: string | null = null;
  let done = false;
  const keypair = Keypair.fromSecret(target.secret);

  const fail = (code: number, message: string) => {
    error ??= message;
    try {
      socket.close(code);
    } catch {
      // Already closing.
    }
  };
  const timer = setTimeout(() => {
    if (stage !== 'session') fail(CLOSE.NORMAL, 'the gateway did not answer the handshake');
  }, HANDSHAKE_TIMEOUT_MS);

  socket.onopen = () => {
    stage = 'admission';
    socket.send(encodeRelayHello(target.room, target.token));
  };
  socket.onmessage = (event: MessageEvent) => {
    if (!(event.data instanceof ArrayBuffer)) {
      fail(CLOSE.PROTOCOL, 'the relay sent a text message');
      return;
    }
    const data = new Uint8Array(event.data);
    try {
      if (stage === 'admission') {
        if (!isHelloOk(data)) {
          fail(CLOSE.PROTOCOL, 'the relay refused the hello');
          return;
        }
        stage = 'handshake';
        const started = Initiator.start({ kind: 'reconnect' }, keypair, target.gatewayPublic, target.hello);
        initiator = started.initiator;
        socket.send(started.msg1);
      } else if (stage === 'handshake') {
        if (initiator === null) return;
        const finished = initiator.finish(data);
        initiator = null;
        session = finished.session;
        session.config.maxMessage = MAX_APP_MESSAGE;
        stage = 'session';
        clearTimeout(timer);
        keypair.wipe();
        events.open();
      } else if (stage === 'session' && session !== null) {
        const message = session.open(data);
        if (message === null) return;
        events.message(decodeRecords(message, 'gateway'), message.length);
      }
    } catch (caught) {
      fail(caught instanceof RecordError ? CLOSE.APP_PROTOCOL : CLOSE.PROTOCOL, describe(caught));
    }
  };
  socket.onclose = (event: CloseEvent) => {
    if (done) return;
    done = true;
    clearTimeout(timer);
    initiator?.abandon();
    keypair.wipe();
    events.closed({ code: event.code, reason: event.reason, stage, error });
  };

  return {
    send(records) {
      if (session === null || socket.readyState !== WebSocket.OPEN) return;
      try {
        for (const frame of session.seal(encodeRecords(records))) socket.send(frame);
      } catch (caught) {
        fail(CLOSE.PROTOCOL, describe(caught));
      }
    },
    close(code) {
      fail(code, error ?? 'closed by the device');
    },
  };
}

// ---- pairing ------------------------------------------------------------------------------------

export interface PairingResult {
  device: string;
  gateway: string;
  token: Uint8Array;
}

export type PairingStage = 'connecting' | 'approval';

export class PairingError extends Error {
  override name = 'PairingError';
  constructor(
    message: string,
    readonly code: number | null,
    /** Refused before the relay accepted the hello (the admission itself). */
    readonly admission: boolean,
  ) {
    super(message);
  }
}

/** How often and how long a pairing admission refused `UNAUTHORIZED` is retried (mobile-relay.md:115). */
export const PAIRING_ADMISSION_RETRIES = 4;
export const PAIRING_ADMISSION_DELAY_MS = 750;

function delay(ms: number): Promise<void> {
  const { promise, resolve } = Promise.withResolvers<void>();
  setTimeout(resolve, ms);
  return promise;
}

/**
 * Runs one pairing (IKpsk1) with a freshly generated key: admission with the invite's one-use
 * token, message 1, the desktop approval wait, message 2. The pairing session carries no
 * application data; the socket is closed as soon as the Welcome arrived.
 */
async function pairOnce(
  invite: PairingInvite,
  keypair: Keypair,
  hello: DeviceHello,
  onStage: (stage: PairingStage) => void,
  socketFactory: SocketFactory,
): Promise<PairingResult> {
  const socket = socketFactory(`${invite.url}${DEVICE_PATH}`);
  socket.binaryType = 'arraybuffer';
  const { promise, resolve, reject } = Promise.withResolvers<PairingResult>();
  let stage: LinkStage = 'socket';
  let initiator: Initiator | null = null;
  let result: PairingResult | null = null;
  let error: string | null = null;

  socket.onopen = () => {
    stage = 'admission';
    socket.send(encodeRelayHello(invite.room, invite.token));
  };
  socket.onmessage = (event: MessageEvent) => {
    try {
      if (!(event.data instanceof ArrayBuffer)) throw new Error('the relay sent a text message');
      const data = new Uint8Array(event.data);
      if (stage === 'admission') {
        if (!isHelloOk(data)) throw new Error('the relay refused the hello');
        stage = 'handshake';
        const started = Initiator.start({ kind: 'pairing', psk: invite.psk }, keypair, invite.gatewayPublic, hello);
        initiator = started.initiator;
        socket.send(started.msg1);
        onStage('approval');
      } else if (stage === 'handshake' && initiator !== null) {
        const { welcome } = initiator.finish(data);
        initiator = null;
        if (welcome.token === undefined) throw new Error('the pairing answer has no admission token');
        result = { device: welcome.device, gateway: welcome.gateway, token: welcome.token };
        stage = 'session';
        socket.close(CLOSE.NORMAL);
      }
    } catch (caught) {
      error = describe(caught);
      socket.close(CLOSE.PROTOCOL);
    }
  };
  socket.onclose = (event: CloseEvent) => {
    initiator?.abandon();
    if (result !== null) {
      resolve(result);
      return;
    }
    const why =
      error ??
      (event.code === CLOSE.UNAUTHORIZED
        ? 'The desktop refused the pairing (denied, expired or already used).'
        : event.code === CLOSE.GATEWAY_OFFLINE
          ? 'The desktop is not connected to the relay.'
          : event.code === CLOSE.RATE_LIMITED
            ? 'The relay is rate limiting; try again shortly.'
            : `The connection closed (${event.code}).`);
    reject(new PairingError(why, event.code, stage === 'admission' || stage === 'socket'));
  };
  return promise;
}

/**
 * Pairs with the desktop that showed `invite`. A pairing admission refused `UNAUTHORIZED` before
 * the relay accepted the hello is retried briefly: the gateway's token set reaches the relay
 * unacknowledged, so a phone that scans within milliseconds may be refused once.
 */
export async function pairWithInvite(
  invite: PairingInvite,
  keypair: Keypair,
  hello: DeviceHello,
  onStage: (stage: PairingStage) => void,
  socketFactory: SocketFactory = browserSocket,
): Promise<PairingResult> {
  for (let attempt = 0; ; attempt++) {
    onStage('connecting');
    try {
      return await pairOnce(invite, keypair, hello, onStage, socketFactory);
    } catch (caught) {
      const retry =
        caught instanceof PairingError &&
        caught.code === CLOSE.UNAUTHORIZED &&
        caught.admission &&
        attempt + 1 < PAIRING_ADMISSION_RETRIES;
      if (!retry) throw caught;
      await delay(PAIRING_ADMISSION_DELAY_MS * (attempt + 1));
    }
  }
}
