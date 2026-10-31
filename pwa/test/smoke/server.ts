// The smoke harness server: one Bun HTTP + WebSocket server on 127.0.0.1 that is
// - the static host of a fresh PWA build (built into a temp dir, never into dist/), whose
//   index.html is rewritten to boot through `boot.ts` (pairing seed, theme, gateway variant);
// - the relay (`/v1/device`: the device hello, answered `HELLO_OK`);
// - the gateway behind it: the real Noise responder (`respond`), the real record codec, and the
//   fixture-backed application side (`gateway.ts`).
// Nothing here is in the production bundle: build.test.ts asserts it.

import { mkdtempSync, readFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import type { ServerWebSocket } from 'bun';
import { build } from '../../build.ts';
import {
  CLOSE,
  DEVICE_PATH,
  decodeRecords,
  encodePairingInvite,
  encodeRecords,
  HELLO_OK,
  Keypair,
  MAX_APP_MESSAGE,
  type Pending,
  respond,
  type Session,
  toBase64Url,
} from '../../src/remote/index.ts';
import { type SmokeConnection, SmokeGateway } from './gateway.ts';

const PWA = join(import.meta.dir, '..', '..');
const SEEDED_DEVICE = 'd-0123456789abcdef';
const GATEWAY_LABEL = 'Workstation';

/** Fixed bytes from a label (deterministic keys and tokens). */
function fixed(label: string, length: number): Uint8Array<ArrayBuffer> {
  const out = new Uint8Array(length);
  const digest = new Bun.CryptoHasher('sha256').update(label).digest();
  for (let i = 0; i < length; i++) out[i] = digest[i % digest.length]! ^ (i >> 5);
  return out;
}

export interface SmokeServer {
  /** `http://127.0.0.1:<port>` */
  origin: string;
  /** The relay URL in invites and the seeded pairing (`ws://127.0.0.1:<port>`). */
  relay: string;
  /** A `v1.…` pairing invite this gateway auto-approves. */
  invite: string;
  gateway: SmokeGateway;
  /** `origin/#route=…&theme=…` */
  url(hash: string): string;
  stop(): Promise<void>;
}

export interface SmokeOptions {
  port?: number;
  /** Build minified (like the release). Default false: readable stack traces. */
  minify?: boolean;
  log?: (line: string) => void;
}

interface SocketData {
  stage: 'hello' | 'handshake' | 'session';
  pairing: boolean;
  pending: Pending | null;
  session: Session | null;
  conn: SmokeConnection | null;
  /** Cookie `smoke_gw=v1`: this connection's gateway announces no `s4`. */
  v1: boolean;
}

export async function startSmoke(options: SmokeOptions = {}): Promise<SmokeServer> {
  const log = options.log ?? (() => undefined);
  const outdir = mkdtempSync(join(tmpdir(), 'ompdeck-smoke-'));
  await build({ outdir, minify: options.minify ?? false });
  const boot = await Bun.build({ entrypoints: [join(import.meta.dir, 'boot.ts')], target: 'browser', minify: false });
  if (!boot.success || boot.outputs[0] === undefined) throw new Error(`boot.ts failed to build: ${boot.logs.join('\n')}`);
  const bootJs = await boot.outputs[0].text();

  const html = readFileSync(join(outdir, 'index.html'), 'utf8');
  const script = /<script type="module"[^>]*src="([^"]+)"[^>]*><\/script>/.exec(html);
  if (script?.[1] === undefined) throw new Error('index.html has no module script');
  const main = script[1].startsWith('/') ? script[1] : `/${script[1].replace(/^\.\//, '')}`;
  const indexHtml = html.replace(script[0], '<script type="module" src="/__smoke/boot.js"></script>');

  const gatewayKey = Keypair.fromSecret(fixed('ompdeck-smoke-gateway', 32));
  const deviceKey = Keypair.fromSecret(fixed('ompdeck-smoke-device', 32));
  const room = fixed('ompdeck-smoke-room', 16);
  const pairingToken = fixed('ompdeck-smoke-pairing-token', 32);
  const deviceToken = fixed('ompdeck-smoke-device-token', 32);
  const psk = fixed('ompdeck-smoke-psk', 32);
  /** Device public key (hex) → registry id. */
  const devices = new Map<string, string>([[Buffer.from(deviceKey.public).toString('hex'), SEEDED_DEVICE]]);

  const vapid = (await crypto.subtle.generateKey({ name: 'ECDSA', namedCurve: 'P-256' }, true, ['sign'])) as CryptoKeyPair;
  const pushKey = toBase64Url(new Uint8Array(await crypto.subtle.exportKey('raw', vapid.publicKey)));
  const png = new Uint8Array(readFileSync(join(PWA, 'public', 'icon-512.png')));
  const gateways = { s4: new SmokeGateway({ s4: true, png, pushKey }), v1: new SmokeGateway({ s4: false, png, pushKey: null }) };

  const server = Bun.serve<SocketData, never>({
    hostname: '127.0.0.1',
    port: options.port ?? 0,
    fetch(request, srv): Response | undefined {
      const url = new URL(request.url);
      if (url.pathname === DEVICE_PATH) {
        const v1 = /(?:^|;\s*)smoke_gw=v1(?:;|$)/.test(request.headers.get('cookie') ?? '');
        const data: SocketData = { stage: 'hello', pairing: false, pending: null, session: null, conn: null, v1 };
        return srv.upgrade(request, { data }) ? undefined : new Response('upgrade failed', { status: 400 });
      }
      if (url.pathname === '/__smoke/boot.js') return new Response(bootJs, { headers: { 'content-type': 'text/javascript' } });
      if (url.pathname === '/__smoke/seed.json') {
        return Response.json({
          main,
          pairing: {
            secret: toBase64Url(deviceKey.secretBytes()),
            gatewayPublic: toBase64Url(gatewayKey.public),
            url: relay,
            room: toBase64Url(room),
            token: toBase64Url(deviceToken),
            device: SEEDED_DEVICE,
            gateway: GATEWAY_LABEL,
            name: 'Pixel 9',
          },
        });
      }
      if (url.pathname === '/' || url.pathname === '/index.html') {
        return new Response(indexHtml, { headers: { 'content-type': 'text/html; charset=utf-8', 'cache-control': 'no-store' } });
      }
      const path = join(outdir, decodeURIComponent(url.pathname));
      if (!path.startsWith(outdir)) return new Response('forbidden', { status: 403 });
      const file = Bun.file(path);
      return file.size > 0 || url.pathname.endsWith('.json') ? new Response(file, { headers: { 'cache-control': 'no-store' } }) : new Response('not found', { status: 404 });
    },
    websocket: {
      maxPayloadLength: 70_000,
      message(ws: ServerWebSocket<SocketData>, message) {
        if (typeof message === 'string') {
          ws.close(CLOSE.PROTOCOL, 'text message');
          return;
        }
        const bytes = new Uint8Array(message);
        const data = ws.data;
        try {
          if (data.stage === 'hello') {
            if (bytes.length !== 50 || bytes[0] !== 0x44 || bytes[1] !== 1 || !equal(bytes.subarray(2, 18), room)) {
              ws.close(CLOSE.UNAUTHORIZED, 'bad hello');
              return;
            }
            data.pairing = equal(bytes.subarray(18, 50), pairingToken);
            data.stage = 'handshake';
            ws.send(HELLO_OK);
            return;
          }
          if (data.stage === 'handshake') {
            const pending = respond(data.pairing ? { kind: 'pairing', psk } : { kind: 'reconnect' }, gatewayKey, bytes);
            const key = Buffer.from(pending.devicePublic).toString('hex');
            if (data.pairing) {
              // The desktop "approves" after a moment, so the approval stage is visible.
              const device = `d-${key.slice(0, 16)}`;
              devices.set(key, device);
              log(`pairing ${pending.hello.name} → ${device}`);
              setTimeout(() => {
                const { msg2 } = pending.accept({ device, gateway: GATEWAY_LABEL, token: deviceToken });
                ws.send(msg2);
              }, 900);
              data.stage = 'session';
              return;
            }
            const device = devices.get(key) ?? `d-${key.slice(0, 16)}`;
            const { session, msg2 } = pending.accept({ device, gateway: GATEWAY_LABEL });
            session.config.maxMessage = MAX_APP_MESSAGE;
            data.session = session;
            data.stage = 'session';
            ws.send(msg2);
            const gateway = data.v1 ? gateways.v1 : gateways.s4;
            log(`device ${device} connected (${data.v1 ? 'v1' : 's4'} gateway, features ${pending.hello.features.join(',') || 'none'})`);
            data.conn = gateway.connect(
              (records) => {
                if (ws.readyState !== 1 || data.session === null) return;
                for (const frame of data.session.seal(encodeRecords(records))) ws.send(frame);
              },
              device,
              pending.hello.name,
            );
            return;
          }
          if (data.session === null || data.conn === null) return;
          const plain = data.session.open(bytes);
          if (plain === null) return;
          data.conn.receive(decodeRecords(plain, 'device'));
        } catch (error) {
          log(`closing a device: ${error instanceof Error ? error.message : String(error)}`);
          ws.close(CLOSE.PROTOCOL, 'protocol error');
        }
      },
      close(ws) {
        ws.data.conn?.close();
      },
    },
  });

  const port: number = server.port ?? 0;
  const origin = `http://127.0.0.1:${port}`;
  const relay: string = `ws://127.0.0.1:${port}`;
  const invite = encodePairingInvite({ url: relay, room, token: pairingToken, gatewayPublic: gatewayKey.public, psk });
  return {
    origin,
    relay,
    invite,
    gateway: gateways.s4,
    url: (hash) => `${origin}/${hash.length === 0 || hash.startsWith('#') ? hash : `#${hash}`}`,
    async stop() {
      await server.stop(true);
      rmSync(outdir, { recursive: true, force: true });
    },
  };
}

function equal(a: Uint8Array, b: Uint8Array): boolean {
  return a.length === b.length && a.every((value, index) => value === b[index]);
}
