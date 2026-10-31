// The phone gate's steps on a real Android phone (`bun run phone`): the live stack (the real relay
// serving the built app, a real gateway, real omp on a scripted model) reached through
// `adb reverse`, so the phone's Chrome loads the app from `http://127.0.0.1:<port>` — loopback is
// a secure context: service worker and Web Push work as on HTTPS. Everything the user does is a
// physical tap or key event on the phone (adb); Chrome's DevTools (CDP over adb) only observes:
// the page's state, console and page errors, WebSockets and CSP violations. Push goes through the
// phone's real push service (FCM) into the system's notification list.

import { mkdirSync } from 'node:fs';
import { join } from 'node:path';
import { type Browser, chromium, type Page } from 'playwright-core';
import { assert, LiveRun, type Stack } from '../live/spec.ts';
import * as phone from './adb.ts';

const OUT = process.env['PHONE_OUT'] ?? '/tmp/pwa-smoke/phone';
const STEP = 30_000;
/** omp's first start in a fresh sandbox (and a turn on the scripted model). */
const SLOW = 120_000;
/** The picked attachment's name in the phone's Download folder. */
const PICKED = 'ompdeck-gate-gradient.png';

interface DeviceRow {
  fingerprint: string;
  control: boolean;
  manage: boolean;
  push: boolean;
}

export class PhoneRun {
  readonly live: LiveRun;
  readonly stack: Stack;
  readonly report: string[] = [];
  readonly shots: string[] = [];
  readonly #devtools: number;
  #browser: Browser | null = null;

  constructor(stack: Stack, devtools: number) {
    this.live = new LiveRun(stack);
    this.stack = stack;
    this.#devtools = devtools;
  }

  note(step: string, observed: string): void {
    const line = `✓ ${step}: ${observed}`;
    this.report.push(line);
    console.log(line);
  }

  /** The phone's whole screen (system UI included). */
  shot(name: string): void {
    const path = join(OUT, `${name}.png`);
    phone.screenshot(path);
    this.shots.push(path);
  }

  /** Attaches DevTools and makes the app's tab current. */
  async attach(): Promise<void> {
    // Seen through the plain HTTP list first: attaching while a fresh tab still starts can hang.
    for (const end = Date.now() + STEP; ; await Bun.sleep(300)) {
      const targets = (await (await fetch(`http://127.0.0.1:${this.#devtools}/json/list`)).json()) as { type: string; url: string }[];
      if (targets.some((t) => t.type === 'page' && t.url.startsWith(this.stack.origin))) break;
      assert(Date.now() < end, `no tab on ${this.stack.origin}`);
    }
    await Bun.sleep(1000);
    const browser = await this.#connect();
    this.#browser = browser;
    const page = browser.contexts()[0]?.pages().find((p) => p.url().startsWith(this.stack.origin));
    assert(page !== undefined, `DevTools shows no tab on ${this.stack.origin}`);
    page.setDefaultTimeout(STEP);
    await this.live.attach(page);
  }

  async #connect(): Promise<Browser> {
    for (let attempt = 1; ; attempt++) {
      try {
        return await chromium.connectOverCDP(`http://127.0.0.1:${this.#devtools}`, { timeout: 10_000 });
      } catch (error) {
        if (attempt === 3) throw error;
        await Bun.sleep(1000);
      }
    }
  }

  /**
   * Detaches DevTools. While it is attached Chrome keeps the page visible, so a backgrounded
   * app would still tell the desktop it is on screen (and get no push).
   */
  async detach(): Promise<void> {
    await this.#browser?.close();
    this.#browser = null;
  }

  /** Clears the origin's site data (pairing, service worker and its push subscription) and closes its tabs. */
  async forget(): Promise<void> {
    const browser = this.#browser ?? (await this.#connect());
    try {
      const context = browser.contexts()[0];
      assert(context !== undefined, 'Chrome has no browser context');
      // Android Chrome refuses Storage.clearDataForOrigin on the browser session: any tab's will do.
      const any = context.pages()[0] ?? (await context.newPage());
      const cdp = await context.newCDPSession(any);
      await cdp.send('Storage.clearDataForOrigin', { origin: this.stack.origin, storageTypes: 'all' });
      await cdp.detach();
      for (const page of context.pages()) {
        if (page.url().startsWith(this.stack.origin)) await page.close();
      }
    } finally {
      await browser.close();
      this.#browser = null;
    }
  }

  /** Taps the conversation's message field, types `text` with the device's keys and taps Send. */
  async send(text: string): Promise<void> {
    const page = this.live.page;
    await page.locator('.composer button.send').waitFor({ timeout: SLOW });
    phone.tap(await phone.waitNode('the message field', (n) => n.pkg === phone.CHROME && n.cls === 'android.widget.EditText' && n.id === ''));
    for (const end = Date.now() + 5000; !phone.keyboardShown(); await Bun.sleep(200)) {
      assert(Date.now() < end, 'the keyboard did not open on the message field');
    }
    phone.typeText(text);
    const field = page.getByLabel('Message');
    for (const end = Date.now() + 5000; (await field.inputValue()) !== text; await Bun.sleep(100)) {
      assert(Date.now() < end, `the field shows “${await field.inputValue()}”, typed “${text}”`);
    }
    // The composer stays above the keyboard.
    const fit = await page.evaluate(() => {
      const box = document.querySelector('.composer')?.getBoundingClientRect();
      const view = window.visualViewport;
      return box === undefined || view === null ? null : { bottom: box.bottom, visible: view.offsetTop + view.height };
    });
    assert(fit !== null && fit.bottom <= fit.visible + 1, `the keyboard covers the composer: ${JSON.stringify(fit)}`);
    await phone.tapNode('Send', (n) => n.desc === 'Send' && n.enabled);
    for (const end = Date.now() + SLOW; (await field.inputValue()) !== ''; await Bun.sleep(200)) {
      const toasts = await page.locator('.toast').allInnerTexts();
      assert(!toasts.some((t) => /did not|failed|refused|not allowed|cannot/i.test(t)) && Date.now() < end, `sending “${text}” did not go through; toasts: ${JSON.stringify(toasts)}`);
    }
    if (phone.keyboardShown()) phone.key('KEYCODE_BACK');
  }

  async idle(): Promise<void> {
    await this.live.page.locator('.composer.mode-send').waitFor({ timeout: SLOW });
  }
}

export async function runPhone(stack: Stack, devtools: number): Promise<PhoneRun> {
  mkdirSync(OUT, { recursive: true });
  const run = new PhoneRun(stack, devtools);
  // Stale state of an earlier run on this origin (pairing with another gateway, subscription).
  await run.forget();
  try {
    await steps(run);
  } catch (error) {
    try {
      run.shot('failure');
    } catch {
      // the screenshot is best effort
    }
    throw error;
  } finally {
    phone.adb('shell', 'cmd', 'statusbar', 'collapse');
    phone.adb('shell', 'rm', '-f', `/sdcard/Download/${PICKED}`);
    await run.detach();
    // Chrome's DevTools socket refuses connections while Chrome is in the background.
    phone.startChrome();
    await Bun.sleep(1500);
    await run.forget().catch((error: unknown) => console.error(`phone: cleanup: ${String(error)}`));
  }
  return run;
}

async function steps(run: PhoneRun): Promise<void> {
  const { stack, live } = run;

  // -- pair: the link opens in Chrome the way a scanned QR code does --
  phone.openUrl(stack.link);
  await run.attach();
  const page = live.page;
  await page.getByRole('button', { name: 'Pair this phone' }).waitFor();
  assert(!page.url().includes('#pair='), `#pair= still in location.href: ${page.url()}`);
  run.shot('01-pair-invite');
  await phone.tapNode('Pair this phone', phone.named('Pair this phone'));
  await page.getByText('Approve on your desktop').waitFor();
  const fingerprint = (await page.locator('.fingerprint').innerText()).trim();
  let desktop: string | null = null;
  for (const end = Date.now() + STEP; desktop === null; ) {
    desktop = (await live.ctl<{ fingerprint: string | null }>('/pairing')).fingerprint;
    if (desktop === null) {
      assert(Date.now() < end, 'the desktop never showed an approval request');
      await Bun.sleep(100);
    }
  }
  run.shot('02-pair-fingerprint');
  assert(fingerprint === desktop, `fingerprints differ: phone ${fingerprint}, desktop ${desktop}`);
  await live.ctl('/approve', { method: 'POST', body: fingerprint });
  await page.getByText(/Connected/).first().waitFor();
  const devices = (await live.ctl<{ devices: (DeviceRow & { platform: string; name: string })[] }>('/mobile')).devices;
  const device = devices.find((d) => d.fingerprint === fingerprint);
  assert(device !== undefined && device.control && device.manage, `the desktop's device: ${JSON.stringify(devices)}`);
  run.note('pair', `link opened by an Android intent; fingerprint ${fingerprint} matched the desktop; approved as “${device.name}” (${device.platform})`);

  // -- the pairing survives a reload (IndexedDB); the device socket runs under the relay's CSP --
  await page.reload();
  await page.getByText(/Connected/).first().waitFor();
  for (const end = Date.now() + STEP; ; await Bun.sleep(200)) {
    if (live.sockets.some((s) => s.url.startsWith(`${stack.relay}/v1/device`) && s.received > 0 && !s.closed)) break;
    assert(Date.now() < end, `no open device WebSocket with traffic: ${JSON.stringify(live.sockets)}`);
  }
  run.shot('03-sessions');
  run.note('reload', 'still paired after a reload; device WebSocket open with traffic');

  // -- push: the user turns it on; Chrome asks; the phone's real push service subscribes --
  await phone.tapNode('Settings', (n) => n.pkg === phone.CHROME && n.cls === 'android.widget.Button' && /^Settings/.test(n.text));
  await phone.tapNode('Notifications', (n) => n.pkg === phone.CHROME && /^Notifications/.test(n.text || n.desc) && n.id === '');
  await page.getByRole('switch', { name: 'Push notifications' }).waitFor();
  await phone.tapNode('the Push notifications switch', (n) => n.pkg === phone.CHROME && n.desc === 'Push notifications');
  const prompt = await phone.waitNode('', phone.named('Allow'), 8000).catch(() => null);
  if (prompt !== null) {
    run.shot('04-permission-prompt');
    phone.tap(prompt);
  }
  await page.getByText('subscribed', { exact: true }).waitFor();
  const endpoint = await page.evaluate(async () => (await (await navigator.serviceWorker.ready).pushManager.getSubscription())?.endpoint ?? '');
  const pushHost = endpoint === '' ? '' : new URL(endpoint).host;
  assert(endpoint.startsWith('https://') && !/^(127\.|localhost)/.test(pushHost), `not a real push service: ${endpoint}`);
  await live.devicePush(fingerprint, true);
  run.shot('05-push-on');
  run.note('push (subscribe)', `${prompt === null ? 'permission already granted on this origin' : 'Chrome asked; Allow tapped'}; subscribed at ${pushHost}; the desktop shows push on`);

  // -- a new session; a prompt typed with the phone's keyboard --
  await phone.tapNode('Back', (n) => n.pkg === phone.CHROME && n.desc === 'Back');
  await phone.tapNode('Sessions', (n) => n.pkg === phone.CHROME && n.cls === 'android.widget.Button' && /^Sessions/.test(n.text));
  await phone.tapNode('New session', phone.named(`New session in ${stack.project}`));
  await run.idle();
  await run.send('PROMPT-MARK typed on the phone');
  const reply = await live.transcript(/LIVE-REPLY to: PROMPT-MARK typed on the phone/);
  await run.idle();
  run.shot('06-prompt-reply');
  run.note('prompt', `typed with key events into the field (keyboard open, composer above it); transcript shows “${reply}”`);

  // -- ask: answered with taps --
  let seen = (await live.requests(0)).count;
  await run.send('ASK-MARK choose one');
  await page.locator('.ui-card', { hasText: 'Pick a color?' }).waitFor({ timeout: SLOW });
  run.shot('07-ask-card');
  await phone.tapNode('the blue option', (n) => n.pkg === phone.CHROME && n.text === 'blue');
  await phone.tapNode('Submit answers', (n) => n.pkg === phone.CHROME && /Submit answers/.test(n.text || n.desc));
  const asked = await live.modelRequest(seen, 'the ask result', (r) => r.last_user.includes('ASK-MARK') && r.messages.some((m) => m.role === 'tool'));
  const toolText = asked.messages.filter((m) => m.role === 'tool').map((m) => m.text).join(' ');
  assert(toolText.includes('blue') && !toolText.includes('red'), `the ask result: ${toolText}`);
  await live.transcript(/ASK-REPLY/);
  await run.idle();
  run.note('ask', `“blue” tapped; the model got “${toolText.slice(0, 60)}”`);

  // -- attachment: picked in Android's own file picker --
  const png = new Uint8Array(await (await fetch(`${stack.control}/png`)).arrayBuffer());
  const local = join(OUT, PICKED);
  await Bun.write(local, png);
  phone.adb('push', local, `/sdcard/Download/${PICKED}`);
  phone.adb('shell', 'am', 'broadcast', '-a', 'android.intent.action.MEDIA_SCANNER_SCAN_FILE', '-d', `file:///sdcard/Download/${PICKED}`);
  await phone.tapNode('Attach files', phone.named('Attach files'));
  // Chrome asks for the camera (its chooser offers a capture option) unless it may already.
  const camera = await phone.waitNode('', phone.named('Only this time'), 3000).catch(() => null);
  if (camera !== null) phone.tap(camera);
  const source = await phone.waitNode('the file chooser', (n) => ['Files', 'Browse', 'Photos & videos', 'Documents'].includes(n.text));
  phone.tap(source);
  await phone.tapNode(PICKED, phone.named(PICKED));
  await page.locator('.attach-chip.done').waitFor();
  run.shot('08-attachment-ready');
  seen = (await live.requests(0)).count;
  await run.send('IMG-MARK what is in this picture');
  const withImage = await live.modelRequest(seen, 'the image', (r) => r.tools && r.last_user.includes('IMG-MARK'));
  const expected = `data:image/png;base64,${Buffer.from(png).toString('base64')}`;
  assert(withImage.images.length === 1 && withImage.images[0] === expected, `the model's image differs (${withImage.images.length} image(s))`);
  await live.transcript(/IMG-REPLY saw 1 image/);
  await run.idle();
  run.note('attachment', `picked through “${source.text}” → ${PICKED}; ${png.length} bytes reached the model byte-identical`);

  // -- push delivery: a turn finishes while the app is in the background --
  assert(phone.notifications(stack.origin).length === 0, `notifications while the app was on screen: ${JSON.stringify(phone.notifications(stack.origin))}`);
  await run.send('PUSH-LONG-MARK take your time');
  await page.locator('.composer.mode-steer').waitFor();
  // Elsewhere in the app, so opening the notification has to navigate.
  await phone.tapNode('Back', (n) => n.pkg === phone.CHROME && n.desc === 'Back');
  await page.getByRole('button', { name: `New session in ${stack.project}` }).waitFor();
  await run.detach();
  // A turn that ends while the app is on screen correctly pushes nothing.
  const controllers = await live.ctl<{ activity: string; label: string }[]>('/live');
  assert(controllers.some((c) => c.activity === 'Busy'), `the turn already ended before the app left the screen: ${JSON.stringify(controllers.map((c) => [c.label, c.activity]))}`);
  phone.key('KEYCODE_HOME');
  const leftAt = Date.now();
  let shown: phone.Shown[] = [];
  for (const end = Date.now() + 60_000; shown.length === 0; await Bun.sleep(500)) {
    shown = phone.notifications(stack.origin);
    assert(shown.length > 0 || Date.now() < end, 'no notification for the finished turn within 60 s');
  }
  const arrived = ((Date.now() - leftAt) / 1000).toFixed(1);
  await Bun.sleep(4000);
  shown = phone.notifications(stack.origin);
  assert(shown.length === 1, `notifications for one finished turn: ${JSON.stringify(shown)}`);
  const notification = shown[0];
  assert(notification !== undefined && notification.title === 'OmpDeck' && notification.text === 'A session finished', `the notification: ${JSON.stringify(notification)}`);
  phone.adb('shell', 'cmd', 'statusbar', 'expand-notifications');
  const host = new URL(stack.origin).host;
  const row = await phone.waitNode('the notification in the shade', (n) => n.text === host);
  run.shot('09-notification');
  const body = phone.nodes().find((n) => n.text === notification.text && Math.abs(n.bounds[1] - row.bounds[3]) < 120);
  assert(body !== undefined, 'the notification text is not in the shade');
  phone.tap(body);
  run.note('push (delivery)', `backgrounded with Home mid-turn; “${notification.title} — ${notification.text}” in the system's list ${arrived} s later (FCM)`);

  // -- opening the notification lands on the session --
  await run.attach();
  await live.page.getByText('PUSH-REPLY finished after a long while').first().waitFor();
  await run.idle();
  assert(phone.notifications(stack.origin).length === 0, 'the notification stayed after it was opened');
  run.shot('10-opened-from-notification');
  run.note('push (open)', 'tapping it brought Chrome back on the session (from the session list); the notification closed');

  // -- the relay restarts: the phone reconnects by itself --
  const socketsBefore = live.sockets.length;
  await live.ctl('/relay-restart', { method: 'POST' });
  const restartedAt = Date.now();
  for (const end = Date.now() + 60_000; ; await Bun.sleep(250)) {
    const fresh = live.sockets.slice(socketsBefore).find((s) => s.url.startsWith(`${stack.relay}/v1/device`) && s.received > 0 && !s.closed);
    if (fresh !== undefined) break;
    assert(Date.now() < end, `no new device WebSocket after the relay restart: ${JSON.stringify(live.sockets)}`);
  }
  const back = ((Date.now() - restartedAt) / 1000).toFixed(1);
  await run.send('RECONNECT-MARK after the restart');
  await live.transcript(/LIVE-REPLY to: RECONNECT-MARK after the restart/);
  run.shot('11-after-relay-restart');
  run.note('reconnect', `relay restarted; the phone opened a new device socket after ${back} s and the next prompt went through`);

  // -- console, page errors, CSP --
  assert(live.csp.length === 0, `CSP violations: ${live.csp.join(' | ')}`);
  assert(live.errors.length === 0, `console/page errors: ${live.errors.join(' | ')}`);
  run.note('csp/console', `0 CSP violations, 0 console errors; ${live.sockets.length} WebSocket(s) seen`);
}
