// End-to-end run of the phone app against the smoke harness (test/smoke): real build, real Noise
// pairing through the fake relay, real wire codec. `bun run e2e` (exit code 1 on any failure).
//
// Kept outside `bun test` (no `.test.ts` name): it needs Chromium and takes ~30 s.

import { mkdirSync, mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import type { BrowserContext, Page } from 'playwright-core';
import { launch, persistentPhone, phone } from '../test/smoke/browser.ts';
import { startSmoke } from '../test/smoke/server.ts';
import type { api } from '../src/remote/index.ts';
import * as fixtures from '../test/smoke/fixtures.ts';

const OUT = process.env['E2E_OUT'] ?? '/tmp/pwa-smoke/e2e';
const TIMEOUT = 15_000;
const treeOnly = process.argv.includes('--tree');
mkdirSync(OUT, { recursive: true });

const smoke = await startSmoke();
const browser = await launch();
const failures: string[] = [];

/**
 * Runs one scenario in a fresh phone context; it fails on any page or console error, and a
 * failure is recorded with a screenshot.
 */
async function scenario(
  name: string,
  hash: string,
  body: (page: Page, context: BrowserContext) => Promise<void>,
  blockWorkers = false,
  /** A real (non-incognito) profile: the Push API exists there, as on a phone. */
  realProfile = false,
): Promise<void> {
  if (treeOnly && !name.startsWith('tree:')) return;
  const profile = realProfile ? mkdtempSync(join(tmpdir(), 'pwa-e2e-profile-')) : null;
  const context = profile === null ? await phone(browser, blockWorkers) : await persistentPhone(profile);
  context.setDefaultTimeout(TIMEOUT);
  const page = await context.newPage();
  const errors: string[] = [];
  page.on('pageerror', (error) => errors.push(error.message));
  page.on('console', (message) => {
    if (message.type() === 'error') errors.push(message.text());
  });
  const started = performance.now();
  try {
    await page.goto(smoke.url(hash));
    await body(page, context);
    if (errors.length > 0) throw new Error(`console errors: ${errors.join(' | ')}`);
    console.log(`✓ ${name} (${Math.round(performance.now() - started)} ms)`);
  } catch (error) {
    const shot = join(OUT, `${name.replace(/\W+/g, '-')}.png`);
    await page.screenshot({ path: shot }).catch(() => undefined);
    failures.push(name);
    console.error(`✗ ${name}: ${error instanceof Error ? error.message : String(error)}\n  screenshot: ${shot}`);
  } finally {
    await context.close();
    if (profile !== null) rmSync(profile, { recursive: true, force: true });
  }
}

async function openSession(page: Page, title: string): Promise<void> {
  await page.getByText(title, { exact: true }).first().click();
  await page.locator('.composer').waitFor();
}

try {
  await scenario('pair with an invite link', `pair=${smoke.invite}`, async (page) => {
    await page.getByRole('button', { name: 'Pair this phone' }).click();
    await page.getByText('Approve on your desktop').waitFor();
    // The harness approves after ~1 s; the app lands on the session list, online.
    await page.getByText('Refactor tmux launcher').first().waitFor();
    await page.getByText(/Connected/).first().waitFor();
  });

  // -- several desktops: one pairing each, one live connection (the active desktop's) --
  const chip = (page: Page, name: string) => page.locator('.desktop-chip', { hasText: name }).first();
  const gatewayConnections = async (page: Page, expected: { first: number; second: number }): Promise<void> => {
    const deadline = Date.now() + TIMEOUT;
    while (smoke.first.gateway.connections !== expected.first || smoke.second.gateway.connections !== expected.second) {
      if (Date.now() > deadline) {
        throw new Error(`device connections: ${smoke.first.label} ${smoke.first.gateway.connections}, ${smoke.second.label} ${smoke.second.gateway.connections}; wanted ${JSON.stringify(expected)}`);
      }
      await page.waitForTimeout(100);
    }
  };
  const openSwitcher = async (page: Page): Promise<void> => {
    await page.locator('.desktop-chip').first().click();
    await page.getByRole('dialog').getByText('Pair another desktop').waitFor();
  };

  await scenario('pair a second desktop, switch between both', 'route=sessions', async (page) => {
    await chip(page, 'Workstation').waitFor();
    await gatewayConnections(page, { first: 1, second: 0 });
    await openSwitcher(page);
    await page.getByRole('dialog').getByRole('button', { name: 'Pair another desktop' }).click();
    await page.getByRole('heading', { name: 'Pair with your desktop' }).waitFor({ state: 'detached' }).catch(() => undefined);
    await page.getByRole('heading', { name: 'Pair another desktop' }).waitFor();
    await page.getByLabel('Pairing link').fill(smoke.second.invite);
    await page.getByText(/Workstation stays paired/).waitFor();
    await page.getByRole('button', { name: 'Pair this desktop' }).click();
    await page.getByText('Approve on your desktop').waitFor();
    await chip(page, 'Laptop').waitFor();
    await page.getByText('Refactor tmux launcher').first().waitFor();
    // The Workstation's connection closed when the Laptop's opened.
    await gatewayConnections(page, { first: 0, second: 1 });

    // Both are listed, the Laptop marked; a tap switches back, with a fresh session list from the Workstation.
    await openSwitcher(page);
    const dialog = page.getByRole('dialog');
    await dialog.locator('button.desktop-option[aria-pressed="true"]', { hasText: 'Laptop' }).waitFor();
    await dialog.locator('button.desktop-option[aria-pressed="false"]', { hasText: 'Workstation' }).click();
    await chip(page, 'Workstation').waitFor();
    await page.getByText('Refactor tmux launcher').first().waitFor();
    await gatewayConnections(page, { first: 1, second: 0 });

    // Settings lists the desktops too.
    await page.getByRole('navigation', { name: 'Main' }).getByRole('button', { name: /Settings/ }).click();
    await page.locator('button.desktop-row', { hasText: 'Laptop' }).click();
    await chip(page, 'Laptop').waitFor();
    await gatewayConnections(page, { first: 0, second: 1 });
  });

  await scenario('a screen is not carried over to the other desktop', 'route=sessions&desktops=2', async (page) => {
    await chip(page, 'Workstation').waitFor();
    await openSession(page, 'Refactor tmux launcher');
    await page.getByLabel('Message').fill('draft for the workstation');
    // A conversation is a pushed screen without the desktop chip: go back, then switch from Settings.
    await page.getByRole('button', { name: 'Back' }).click();
    await page.getByRole('navigation', { name: 'Main' }).getByRole('button', { name: /Settings/ }).click();
    await page.locator('button.desktop-row', { hasText: 'Laptop' }).click();
    await chip(page, 'Laptop').waitFor();
    if ((await page.locator('.composer').count()) !== 0) throw new Error('the other desktop opened with the previous conversation');
    if ((await page.getByText('draft for the workstation').count()) !== 0) throw new Error('a draft followed the switch');
  });

  await scenario('re-pairing a paired desktop from an invite link replaces it, others stay', 'route=sessions&desktops=2', async (page) => {
    await chip(page, 'Workstation').waitFor();
    await page.evaluate((invite) => {
      location.hash = `#pair=${invite}`;
    }, smoke.first.invite);
    await page.getByRole('heading', { name: 'Pair again with Workstation' }).waitFor();
    await page.getByText(/replaces this phone’s pairing with Workstation/).waitFor();
    // Nothing dropped yet: the live connection is still the Workstation's.
    await gatewayConnections(page, { first: 1, second: 0 });
    await page.getByRole('button', { name: 'Replace pairing' }).click();
    await page.getByText('Approve on your desktop').waitFor();
    await chip(page, 'Workstation').waitFor();
    await openSwitcher(page);
    const options = page.getByRole('dialog').locator('button.desktop-option');
    const names = await options.allInnerTexts();
    if (names.length !== 2 || names.filter((text) => text.includes('Workstation')).length !== 1 || names.filter((text) => text.includes('Laptop')).length !== 1) {
      throw new Error(`desktops after re-pairing: ${JSON.stringify(names)}`);
    }
  });

  await scenario('an invite link for a new desktop opened while the app runs keeps the others', 'route=sessions', async (page) => {
    await chip(page, 'Workstation').waitFor();
    await page.evaluate((invite) => {
      location.hash = `#pair=${invite}`;
    }, smoke.second.invite);
    await page.getByRole('heading', { name: 'Pair another desktop' }).waitFor();
    await page.getByText(/Workstation stays paired/).waitFor();
    await gatewayConnections(page, { first: 1, second: 0 });
    // Cancel: back to the Workstation, still connected.
    await page.getByRole('button', { name: 'Cancel' }).click();
    await chip(page, 'Workstation').waitFor();
    await gatewayConnections(page, { first: 1, second: 0 });
  });

  await scenario('forget one desktop: the next one becomes active; forget the last: pairing screen', 'route=sessions&desktops=2', async (page) => {
    await chip(page, 'Workstation').waitFor();
    const forgetHere = async (): Promise<void> => {
      await page.getByRole('navigation', { name: 'Main' }).getByRole('button', { name: /Settings/ }).click();
      await page.locator('button.hub-row', { hasText: 'This device' }).click();
      await page.getByRole('button', { name: 'Forget on this phone only' }).click();
      await page.getByRole('button', { name: 'Forget', exact: true }).click();
    };
    await forgetHere();
    await chip(page, 'Laptop').waitFor();
    await gatewayConnections(page, { first: 0, second: 1 });
    await page.getByText('Refactor tmux launcher').first().waitFor().catch(() => undefined);
    await forgetHere();
    await page.getByRole('heading', { name: 'Pair with your desktop' }).waitFor();
    await gatewayConnections(page, { first: 0, second: 0 });
  });

  await scenario('a notification tap for the other desktop switches to it and opens its session', 'route=sessions&desktops=2', async (page) => {
    await chip(page, 'Workstation').waitFor();
    // What the service worker opens (or posts) for a push from the Laptop: its id first, then the route.
    await page.evaluate((id) => {
      location.hash = `#d=${id}&s=s-convo`;
    }, smoke.second.id);
    await page.locator('.composer').waitFor();
    await gatewayConnections(page, { first: 0, second: 1 });
    await page.getByRole('button', { name: 'Back' }).click();
    await chip(page, 'Laptop').waitFor();
    // A link naming a desktop this phone is not paired with is ignored: still on the Laptop, nothing opened.
    await page.evaluate(() => {
      location.hash = `#d=${'ee'.repeat(32)}&s=s-idle`;
    });
    await page.waitForTimeout(300);
    await chip(page, 'Laptop').waitFor();
    if ((await page.locator('.composer').count()) !== 0) throw new Error('a link for an unpaired desktop opened a conversation');
    await gatewayConnections(page, { first: 0, second: 1 });
  });

  await scenario('a version 1 pairing migrates without pairing again', 'route=sessions&legacy=1', async (page) => {
    await chip(page, 'Workstation').waitFor();
    await page.getByText('Refactor tmux launcher').first().waitFor();
    await gatewayConnections(page, { first: 1, second: 0 });
  });

  // -- the upgrade while the old app is still open in another window --
  // Version 1 keeps its IndexedDB connection for the page's life and cannot be told to let go, so it
  // blocks the new version's upgrade of the database. The holder page (test/smoke/holder.ts) stands
  // in for it. The first page of each scenario installs the worker, as the app does on its first run.
  const holderUrl = new URL('/__smoke/holder', smoke.url('')).href;
  const controlled = (page: Page) => page.waitForFunction(() => navigator.serviceWorker.controller !== null);
  const watch = (page: Page, errors: string[]) => {
    page.on('pageerror', (error) => errors.push(error.message));
    page.on('console', (message) => {
      if (message.type() === 'error') errors.push(message.text());
    });
  };

  await scenario('an old window holding the database is reloaded by the worker, and the new app boots paired', 'route=sessions', async (page, context) => {
    await controlled(page);
    // This window becomes the old app: it drops the new database and holds version 1 instead.
    await page.goto(holderUrl);
    await page.locator('body[data-holding="yes"]').waitFor({ state: 'attached' });
    if (!(await page.evaluate(() => navigator.serviceWorker.controller !== null))) throw new Error('the old window is not controlled by the worker');
    const errors: string[] = [];
    const fresh = await context.newPage();
    watch(fresh, errors);
    await fresh.goto(smoke.url('route=sessions&legacy=1'));
    await chip(fresh, 'Workstation').waitFor();
    await fresh.getByText('Refactor tmux launcher').first().waitFor();
    // The worker reloaded the old window: it ran again and let go.
    await page.waitForFunction(() => document.body?.dataset['loads'] === '2');
    if (errors.length > 0) throw new Error(`console errors: ${errors.join(' | ')}`);
  });

  await scenario('an old window the worker cannot reload: the app says it is finishing the update and boots once that window closes', 'route=sessions', async (page, context) => {
    await controlled(page);
    await page.goto('about:blank');
    // Not controlled by the worker (it is bypassed for this page), so it cannot be navigated.
    const old = await context.newPage();
    const session = await context.newCDPSession(old);
    await session.send('Network.enable');
    await session.send('Network.setBypassServiceWorker', { bypass: true });
    await old.goto(holderUrl);
    await old.locator('body[data-holding="yes"]').waitFor({ state: 'attached' });
    if (await old.evaluate(() => navigator.serviceWorker.controller !== null)) throw new Error('the old window is controlled by the worker');
    const errors: string[] = [];
    const fresh = await context.newPage();
    watch(fresh, errors);
    await fresh.goto(smoke.url('route=sessions&legacy=1'));
    await fresh.getByRole('heading', { name: 'Finishing the update…' }).waitFor();
    await fresh.getByText('close the other OmpDeck tabs').waitFor();
    // Give the worker its chance: the window stays as it was.
    await fresh.waitForTimeout(500);
    if ((await old.evaluate(() => document.body.dataset['loads'])) !== '1') throw new Error('the worker reloaded a window it does not control');
    if ((await fresh.locator('.desktop-chip').count()) !== 0) throw new Error('the app booted while the database was blocked');
    await old.close();
    await chip(fresh, 'Workstation').waitFor();
    await fresh.getByText('Refactor tmux launcher').first().waitFor();
    if (errors.length > 0) throw new Error(`console errors: ${errors.join(' | ')}`);
  });

  // -- Web Push per desktop: each desktop's subscription is on its own registration (/d/<id>/) --
  await scenario(
    'push: each desktop subscribes on its own service-worker registration; forgetting one removes only its own',
    'route=settings&desktops=2',
    async (page, context) => {
      await context.grantPermissions(['notifications'], { origin: smoke.origin });
      // The real push service is out of reach of headless Chromium: one fake subscription per registration's PushManager.
      await page.evaluate(() => {
        const subs = new WeakMap<object, PushSubscription>();
        const make = (n: number, key: ArrayBuffer): PushSubscription =>
          ({
            endpoint: `https://push.example/${n}`,
            expirationTime: null,
            options: { userVisibleOnly: true, applicationServerKey: key },
            getKey: () => new Uint8Array([1, 2, 3]).buffer,
            unsubscribe: async function (this: PushSubscription) {
              for (const manager of [...registry]) if (subs.get(manager) === this) subs.delete(manager);
              return true;
            },
            toJSON: () => ({ endpoint: `https://push.example/${n}`, expirationTime: null, keys: { p256dh: 'AQID', auth: 'AQID' } }),
          }) as unknown as PushSubscription;
        const registry = new Set<object>();
        let count = 0;
        PushManager.prototype.subscribe = async function (options?: PushSubscriptionOptionsInit): Promise<PushSubscription> {
          registry.add(this);
          const key = options?.applicationServerKey;
          const buffer = key instanceof ArrayBuffer ? key : key !== null && key !== undefined && typeof key !== 'string' ? (key as ArrayBufferView).buffer.slice(0) as ArrayBuffer : new ArrayBuffer(0);
          const made = make(++count, buffer);
          subs.set(this, made);
          return made;
        };
        PushManager.prototype.getSubscription = async function (): Promise<PushSubscription | null> {
          return subs.get(this) ?? null;
        };
      });
      const scopes = () =>
        page.evaluate(async () => {
          const found: { scope: string; subscribed: boolean }[] = [];
          for (const registration of await navigator.serviceWorker.getRegistrations()) {
            found.push({ scope: new URL(registration.scope).pathname, subscribed: (await registration.pushManager.getSubscription()) !== null });
          }
          return found.sort((a, b) => a.scope.localeCompare(b.scope));
        });
      const notifications = async (): Promise<void> => {
        await page.getByRole('navigation', { name: 'Main' }).getByRole('button', { name: /Settings/ }).click();
        await page.locator('button.hub-row', { hasText: 'Notifications' }).click();
        await page.getByRole('switch', { name: 'Push notifications' }).waitFor();
      };

      await chip(page, 'Workstation').waitFor();
      await page.evaluate(() => navigator.serviceWorker.ready);
      await notifications();
      await page.getByRole('switch', { name: 'Push notifications' }).click();
      await page.getByText('subscribed', { exact: true }).waitFor();
      let found = await scopes();
      const subscribed = found.filter((entry) => entry.subscribed).map((entry) => entry.scope);
      if (subscribed.length !== 1 || subscribed[0] !== `/d/${smoke.first.id}/`) throw new Error(`after enabling push for the Workstation: ${JSON.stringify(found)}`);

      // The Laptop is its own desktop: not subscribed until its own switch is turned on.
      await page.getByRole('button', { name: 'Back' }).click();
      await page.locator('button.desktop-row', { hasText: 'Laptop' }).click();
      await chip(page, 'Laptop').waitFor();
      await notifications();
      await page.getByText('not subscribed', { exact: true }).waitFor();
      await page.getByRole('switch', { name: 'Push notifications' }).click();
      await page.getByText('subscribed', { exact: true }).waitFor();
      found = await scopes();
      const both = found.filter((entry) => entry.subscribed).map((entry) => entry.scope).sort();
      if (JSON.stringify(both) !== JSON.stringify([`/d/${smoke.first.id}/`, `/d/${smoke.second.id}/`].sort())) throw new Error(`after enabling push for both: ${JSON.stringify(found)}`);

      // Forget the Laptop: its registration goes, the Workstation's stays subscribed.
      await page.getByRole('button', { name: 'Back' }).click();
      await page.locator('button.hub-row', { hasText: 'This device' }).click();
      await page.getByRole('button', { name: 'Forget on this phone only' }).click();
      await page.getByRole('button', { name: 'Forget', exact: true }).click();
      await chip(page, 'Workstation').waitFor();
      const deadline = Date.now() + TIMEOUT;
      for (;;) {
        found = await scopes();
        if (!found.some((entry) => entry.scope === `/d/${smoke.second.id}/`)) break;
        if (Date.now() > deadline) throw new Error(`the Laptop's registration survived forgetting it: ${JSON.stringify(found)}`);
        await page.waitForTimeout(100);
      }
      const left = found.filter((entry) => entry.subscribed).map((entry) => entry.scope);
      if (left.length !== 1 || left[0] !== `/d/${smoke.first.id}/`) throw new Error(`after forgetting the Laptop: ${JSON.stringify(found)}`);
    },
    false,
    true,
  );

  const treeSession = 's-idle';
  const treeTitle = 'Refactor tmux launcher';
  const treeConversation = `route=conversation&session=${treeSession}&title=${encodeURIComponent(treeTitle)}`;
  const gw = smoke.gateway;
  const sample = fixtures.tree(gw.t0, treeSession, 'default', null, 1).nodes[0];
  if (sample === undefined) throw new Error('missing tree fixture');
  const windowNodes: api.TreeNode[] = Array.from({ length: 60 }, (_, i) => ({
    ...sample, id: `window-${i}`, parent: i === 0 ? null : `window-${i - 1}`, text: `Window entry ${i}`,
  }));
  const treeRequests: Extract<api.Op, { op: 'tree' }>[] = [];
  const extraBeforeTree = gw.extraItems.get(treeSession);
  try {
    gw.treeNav = true;
    let leaf = 'window-45';
    gw.treeListing = (op) => {
      treeRequests.push(op);
      const [start, end] = op.cursor === 'b-24' ? [0, 24] : op.cursor === 't-54' ? [54, 60] : [24, 54];
      return {
        type: 'tree', nodes: windowNodes.slice(start, end), active_leaf: leaf, truncated: false, total: windowNodes.length,
        prev_cursor: start === 0 ? null : `b-${start}`,
        next_cursor: end === windowNodes.length ? null : `t-${end}`,
      };
    };
    gw.navigate = (op) => {
      if (op.entry === 'window-43') {
        return { type: 'moved', leaf, before_prompt: false, editor_text: null, dropped_images: 0, message: 'Already at this point' };
      }
      leaf = op.entry === 'window-42' ? 'window-41' : op.entry;
      gw.extraItems.set(treeSession, [{ type: 'user', text: 'Reached the new branch', from_agent: false, at_unix_ms: null }]);
      gw.changed('transcript');
      return {
        type: 'moved', leaf, before_prompt: op.entry === 'window-42', dropped_images: op.entry === 'window-42' ? 1 : 0,
        editor_text: op.entry === 'window-42' ? 'Original prompt text' : null, message: null,
      };
    };
    await scenario('tree: windowed paging, no-op notice, branch jump and existing draft', treeConversation, async (page) => {
      gw.extraItems.set(treeSession, [{ type: 'user', text: 'Abandoned turn', from_agent: false, at_unix_ms: null }]);
      gw.changed('transcript');
      await page.getByText('Abandoned turn', { exact: true }).waitFor();
      await page.getByLabel('Message').fill('Keep my phone draft');
      await page.getByRole('button', { name: 'Session actions' }).click();
      await page.getByRole('button', { name: /Session tree/ }).click();

      const body = page.locator('.tree-screen .body');
      const here = body.locator('#tree-here');
      await here.getByText('◉ you are here').waitFor();
      const centered = await here.evaluate((row) => {
        const viewport = row.closest('.body')?.getBoundingClientRect();
        const rect = row.getBoundingClientRect();
        return viewport !== undefined && Math.abs(rect.top + rect.bottom - viewport.top - viewport.bottom) < 80;
      });
      if (!centered || (await page.locator('.tree-screen .count').innerText()) !== '60') throw new Error('initial window did not center the leaf or show the total');
      const earlier = page.getByRole('button', { name: 'Load earlier entries' });
      const first = page.getByRole('button', { name: 'You: Window entry 24', exact: true });
      await earlier.scrollIntoViewIfNeeded();
      const top = (await first.boundingBox())?.y;
      await earlier.click();
      await page.getByRole('button', { name: 'You: Window entry 0', exact: true }).waitFor();
      const newTop = (await first.boundingBox())?.y;
      if (top === undefined || newTop === undefined || Math.abs(top - newTop) > 4) throw new Error(`prepend shifted the old window: ${top} → ${newTop}`);
      await page.getByRole('button', { name: 'Load more entries' }).click();
      await page.getByRole('button', { name: 'You: Window entry 59', exact: true }).waitFor();
      await page.getByRole('button', { name: 'Conversation', exact: true }).click();
      await page.getByRole('button', { name: 'You: Window entry 0', exact: true }).waitFor({ state: 'detached' });

      await page.getByRole('button', { name: 'You: Window entry 43', exact: true }).click();
      await page.getByRole('button', { name: /Go here/ }).click();
      await page.getByText('Already at this point', { exact: true }).waitFor();
      await page.getByRole('dialog').getByRole('button', { name: 'Close' }).click();
      await page.getByRole('button', { name: 'You: Window entry 42', exact: true }).click();
      await page.getByRole('button', { name: /Go here/ }).click();
      await page.getByText(/Moved before that prompt — its text is in the composer.*1 image of that prompt was not carried over/).waitFor();
      const message = await page.getByLabel('Message').inputValue();
      if (message !== 'Original prompt text\n\nKeep my phone draft') throw new Error(`jump lost the composer text: ${JSON.stringify(message)}`);
      await page.getByText('Reached the new branch', { exact: true }).waitFor();
      await page.getByText('Abandoned turn', { exact: true }).waitFor({ state: 'detached' });
      await page.getByRole('button', { name: 'Session actions' }).click();
      await page.getByRole('button', { name: /Session tree/ }).click();
      await page.locator('#tree-here').getByRole('button', { name: 'You: Window entry 41', exact: true }).waitFor();
      await page.getByRole('button', { name: 'Back' }).click();
      await page.getByLabel('Message').waitFor();
      await page.getByRole('button', { name: 'Back' }).click();
      await page.getByRole('navigation', { name: 'Main' }).waitFor();

      // StrictMode may repeat a mount's initial request in this unminified build.
      const ops = treeRequests.map((op) => JSON.stringify([op.filter, op.cursor ?? null, op.at_leaf ?? null]))
        .filter((op, i, all) => op !== all[i - 1]);
      const wanted = [
        ['default', null, true], ['default', 'b-24', null], ['default', 't-54', null], ['conversation', null, true],
        ['default', null, true],
      ];
      if (JSON.stringify(ops) !== JSON.stringify(wanted.map((op) => JSON.stringify(op)))) throw new Error(`unexpected tree requests: ${ops.join(', ')}`);
    });

    await scenario('tree: jump from the fleet replaces the tree with the live conversation', 'route=sessions', async (page) => {
      await page.getByRole('button', { name: `Actions for ${treeTitle}`, exact: true }).first().click();
      await page.getByRole('button', { name: /Session tree/ }).click();
      await page.getByRole('button', { name: 'You: Window entry 38', exact: true }).click();
      await page.getByRole('button', { name: /Go here/ }).click();
      await page.getByText('Moved there — new prompts continue from this point', { exact: true }).waitFor();
      await page.getByLabel('Message').waitFor();
      await page.getByRole('button', { name: 'Back' }).click();
      await page.getByRole('navigation', { name: 'Main' }).waitFor();
    });

    await scenario('tree: leaving a pending jump keeps the new route and receives its outcome', treeConversation, async (page) => {
      const pending = Promise.withResolvers<void>();
      gw.navigateDelay = pending.promise;
      try {
        await page.getByLabel('Message').fill('Kept while the request is pending');
        await page.getByRole('button', { name: 'Session actions' }).click();
        await page.getByRole('button', { name: /Session tree/ }).click();
        await page.getByRole('button', { name: 'You: Window entry 42', exact: true }).click();
        await page.getByRole('button', { name: /Go here/ }).click();
        await page.getByRole('button', { name: /Go here/, disabled: true }).waitFor();
        await page.goBack();
        await page.getByRole('button', { name: '± Changes', exact: true }).click();
        await page.getByText('Changes', { exact: true }).waitFor();
        const route = await page.evaluate(() => JSON.stringify(history.state.stack));

        pending.resolve();
        await page.getByText(/Moved before that prompt — its text is in the composer/).waitFor();
        if (await page.evaluate(() => JSON.stringify(history.state.stack)) !== route) throw new Error('late jump replaced or popped the current route');
        await page.getByText('Changes', { exact: true }).waitFor();
        await page.getByRole('button', { name: 'Back', exact: true }).click();
        const message = await page.getByLabel('Message').inputValue();
        if (message !== 'Original prompt text\n\nKept while the request is pending') throw new Error(`late jump lost its prefill: ${JSON.stringify(message)}`);
      } finally {
        pending.resolve();
        gw.navigateDelay = null;
      }
    });

    await scenario('tree: a late jump after controller reuse opens only the moved session', 'route=sessions', async (page) => {
      const pending = Promise.withResolvers<void>();
      const original = gw.liveSessions().find((entry) => entry.session === treeSession);
      if (original === undefined) throw new Error('missing original live session');
      gw.navigateDelay = pending.promise;
      try {
        const actions = page.getByRole('button', { name: `Actions for ${treeTitle}`, exact: true }).first();
        await actions.click();
        await page.getByRole('button', { name: /Session tree/ }).click();
        await page.getByRole('button', { name: 'You: Window entry 42', exact: true }).click();
        await page.getByRole('button', { name: /Go here/ }).click();
        const dialog = page.getByRole('dialog');
        const forkHere = dialog.getByRole('button', { name: /Fork here/ });
        if (!await forkHere.isDisabled()) throw new Error('the jump is not pending');
        gw.liveOverrides.set(treeSession, {
          ...original, session: 's-reused', title: 'Replacement session',
          live: { ...original.live, generation: original.live.generation + 1 },
        });
        gw.changed('live');
        await dialog.getByText('Go here needs the session running.').waitFor();
        pending.resolve();

        await page.getByText(/Moved before that prompt — its text was not applied because the session is no longer running there/).waitFor();
        await page.getByRole('button', { name: 'Resume', exact: true }).waitFor();
        const route = await page.evaluate(() => history.state.stack);
        if (JSON.stringify(route) !== JSON.stringify([{ kind: 'conversation', session: treeSession, liveKey: null, title: treeTitle }])) {
          throw new Error(`late jump attached the moved session to the reused controller: ${JSON.stringify(route)}`);
        }
        await page.getByRole('button', { name: 'Back', exact: true }).click();
        await openSession(page, 'Replacement session');
        if (await page.getByLabel('Message').inputValue() !== '') throw new Error('the moved prompt leaked into the replacement session');
      } finally {
        pending.resolve();
        gw.navigateDelay = null;
        gw.liveOverrides.delete(treeSession);
      }
    });
  } finally {
    gw.treeNav = false;
    gw.treeListing = null;
    gw.navigate = null;
    if (extraBeforeTree === undefined) gw.extraItems.delete(treeSession);
    else gw.extraItems.set(treeSession, extraBeforeTree);
  }

  await scenario('tree: a legacy gateway keeps Go here on the tree', treeConversation, async (page) => {
    gw.treeListing = (op) => {
      treeRequests.push(op);
      return fixtures.tree(gw.t0, op.session, op.filter, op.cursor ?? null, op.limit);
    };
    try {
      await page.getByRole('button', { name: 'Session actions' }).click();
      await page.getByRole('button', { name: /Session tree/ }).click();
      await page.getByRole('button', { name: 'You: Start with the wire types', exact: true }).click();
      await page.getByRole('button', { name: /Go here/ }).click();
      await page.locator('.sheet').waitFor({ state: 'detached' });
      await page.getByRole('list', { name: 'Session entries' }).waitFor();
      if (treeRequests.at(-1)?.at_leaf !== undefined) throw new Error('sent at_leaf without tree_nav');
      if (await page.getByRole('button', { name: 'Load earlier entries' }).count() !== 0) throw new Error('legacy tree advertised backwards paging');
    } finally {
      gw.treeListing = null;
    }
  });

  await scenario('tree: a transcript reset replaces earlier pages and returns to the bottom', treeConversation, async (page) => {
    const older = gw.extraItems.get(treeSession);
    const history = (branch: string): api.Item[] => Array.from({ length: 140 }, (_, i) => ({
      type: 'user', text: `${branch} turn ${i}`, from_agent: false, at_unix_ms: null,
    }));
    try {
      gw.extraItems.set(treeSession, history('Old'));
      gw.changed('transcript');
      const earlier = page.getByRole('button', { name: 'Load earlier messages' });
      await earlier.click();
      const first = page.getByText('Old turn 0', { exact: true });
      await first.scrollIntoViewIfNeeded();
      gw.extraItems.set(treeSession, history('New'));
      gw.changed('transcript');
      await earlier.waitFor();
      await first.waitFor({ state: 'detached' });
      await page.getByText('New turn 139', { exact: true }).waitFor();
      await page.waitForFunction(() => {
        const scroller = document.querySelector('.chat-content');
        return scroller !== null && scroller.scrollHeight - scroller.scrollTop - scroller.clientHeight < 4;
      });
    } finally {
      if (older === undefined) gw.extraItems.delete(treeSession);
      else gw.extraItems.set(treeSession, older);
    }
  });

  for (const from of ['entry', 'conversation', 'failed start'] as const) {
    await scenario(`tree: fork from ${from}`, treeConversation, async (page) => {
      const forkId = `${treeSession}-${from === 'entry' ? 'fork' : 'copy'}`;
      gw.forkResumeFailure = from === 'failed start' ? 'omp could not start on the fixture' : null;
      try {
        await page.getByLabel('Message').waitFor();
        await page.getByRole('button', { name: 'Session actions' }).click();
        if (from === 'entry') {
          await page.getByRole('button', { name: /Session tree/ }).click();
          await page.getByRole('button', { name: 'You: Start with the wire types', exact: true }).click();
          await page.getByRole('button', { name: /Fork here/ }).click();
        } else {
          await page.getByRole('button', { name: /Fork session/ }).click();
          await page.getByRole('button', { name: 'Fork', exact: true }).click();
        }
        await page.getByRole('button', { name: 'Starting the fork…', exact: true }).waitFor();
        await page.getByRole('heading', { name: `${treeTitle} (fork)`, exact: true }).waitFor();
        if (from === 'failed start') {
          await page.getByText(/Start the fork failed: omp could not start on the fixture/).waitFor();
          await page.getByRole('button', { name: 'Resume', exact: true }).waitFor();
        } else {
          await page.getByLabel('Message').waitFor();
        }
        const screen = await page.evaluate(() => history.state.stack.at(-1) as { session: string; liveKey: string | null });
        const liveKey = from === 'failed start' ? null : `tmux:8:omp-deck:${forkId}`;
        if (screen.session !== forkId || screen.liveKey !== liveKey) throw new Error(`opened the wrong fork: ${JSON.stringify(screen)}`);
      } finally {
        gw.forkResumeFailure = null;
        gw.forks.delete(forkId);
      }
    });
  }

  await scenario('send a prompt and see the reply', 'route=sessions', async (page) => {
    await openSession(page, 'Refactor tmux launcher');
    await page.getByLabel('Message').fill('Please split launch.rs');
    await page.locator('.composer button.send').click();
    await page.getByText('Please split launch.rs').first().waitFor();
    await page.getByText('Starting with the files you mentioned.').first().waitFor();
  });

  await scenario('answer a pending confirmation', 'route=conversation&session=s-ui-confirm&title=Clean%20old%20worktrees', async (page) => {
    const card = page.locator('.ui-card');
    await card.first().waitFor();
    await card.getByRole('button', { name: '✓ Yes' }).click();
    await card.first().waitFor({ state: 'detached' });
  });

  await scenario('download: transcript image and HTML export', 'route=conversation&session=s-convo&title=Ship%20mobile%20relay', async (page) => {
    // The transcript image is pulled chunk by chunk and shown as a thumbnail.
    await page.locator('button.img-thumb img').first().waitFor();
    await page.getByRole('button', { name: 'Back' }).click();
    await openSession(page, 'Refactor tmux launcher');
    await page.getByRole('button', { name: 'Session actions' }).click();
    const [file] = await Promise.all([page.waitForEvent('download'), page.getByText('Export as HTML').click()]);
    if (!file.suggestedFilename().endsWith('.html')) throw new Error(`export saved as ${file.suggestedFilename()}`);
    const path = await file.path();
    const html = await Bun.file(path).text();
    if (!html.includes('<html')) throw new Error('the export is not HTML');
  });

  await scenario('attach a file and send it', 'route=conversation&session=s-idle&title=Refactor%20tmux%20launcher', async (page) => {
    await page.locator('.composer input[type=file]:not([capture])').setInputFiles({
      name: 'notes.txt',
      mimeType: 'text/plain',
      buffer: Buffer.alloc(700_000, 'omp '),
    });
    await page.locator('.attach-chip.done').waitFor();
    await page.getByLabel('Message').fill('Read the notes');
    await page.locator('.composer button.send').click();
    await page.getByText('I can see notes.txt').first().waitFor();
  });

  for (const section of ['backup', 'sync'] as const) {
    await scenario(`${section}: destination switches reset selection and fence old replies`, `route=settings&section=${section}`, async (page) => {
      const gw = smoke.gateway;
      const reply = (marker: string): api.Resp => section === 'backup'
        ? { type: 'backup_list', hosts: [{ host: marker, backups: [{ id: marker, name: marker, size: 1, created_unix_ms: gw.t0 }] }], truncated: false }
        : { type: 'sync_backups', backups: [{ ...fixtures.syncBackups(gw.t0, null)[0]!, id: marker, name: marker, host: marker }], truncated: false };
      const reload = page.getByRole('button', { name: section === 'backup' ? 'Reload versions' : 'Reload backups', exact: true });
      await page.locator('.version .check').first().click();
      await page.locator('.select-bar').waitFor();
      const held = Promise.withResolvers<api.Resp>();
      const pending = Promise.withResolvers<void>();
      gw.backupListing = () => {
        pending.resolve();
        return held.promise;
      };
      await reload.click();
      const timer = setTimeout(() => pending.reject(new Error('old listing was not requested')), TIMEOUT);
      try {
        await pending.promise;
      } finally {
        clearTimeout(timer);
      }
      try {
        for (const [index, destination] of [
          { kind: 'sftp', label: 'sftp://first.example' },
          { kind: 'sftp', label: 'sftp://second.example' },
        ].entries()) {
          const marker = `destination-version-${index}`;
          gw.backupListing = () => reply(marker);
          gw.backupDestination = destination as api.BackupDestinationView;
          // Do not change timestamps: kind/label alone must cause a fresh listing.
          gw.changed('backup');
          await page.getByRole('button', { name: `Select ${marker}`, exact: true }).waitFor();
          if (await page.locator('.select-bar').count() !== 0) throw new Error('selection survived a destination switch');
          if (index === 0) {
            held.resolve(reply('stale-version'));
            await page.waitForTimeout(250);
            if (await page.getByRole('button', { name: 'Select stale-version', exact: true }).count() !== 0) throw new Error('old response replaced destination listing');
          }
          await page.getByRole('button', { name: `Select ${marker}`, exact: true }).click();
          await page.locator('.select-bar').waitFor();
        }
      } finally {
        gw.backupListing = null;
        gw.backupDestination = { kind: 'drive', label: 'Google Drive' };
        gw.changed('backup');
      }
    });
  }

  await scenario('terminal input reaches the desktop and echoes', 'route=terminal&live=tmux%3A8%3Aomp-deck%3Aomp-3c4d&title=Refactor%20tmux%20launcher', async (page) => {
    await page.getByRole('button', { name: /Observe/ }).click();
    await page.locator('button.option', { hasText: 'Type into the session' }).click();
    const input = page.locator('form.term-input input');
    await input.fill('git status');
    await input.press('Enter');
    const deadline = Date.now() + TIMEOUT;
    while (!smoke.gateway.terminalInput.includes('git status\r')) {
      if (Date.now() > deadline) throw new Error(`the desktop got ${JSON.stringify(smoke.gateway.terminalInput)}`);
      await page.waitForTimeout(100);
    }
    await page.locator('.term').getByText('omp is thinking').first().waitFor();
  });

  await scenario(
    'an older desktop without service workers shows the update hint',
    'route=settings&section=usage&gw=v1',
    async (page) => {
      await page.locator('.needs-s4').first().waitFor();
    },
    true,
  );
} finally {
  await browser.close();
  await smoke.stop();
}

if (failures.length > 0) {
  console.error(`${failures.length} e2e scenario(s) failed`);
  process.exit(1);
}
console.log('e2e: all scenarios passed');
