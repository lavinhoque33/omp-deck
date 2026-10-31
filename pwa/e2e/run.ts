// End-to-end run of the phone app against the smoke harness (test/smoke): real build, real Noise
// pairing through the fake relay, real wire codec. `bun run e2e` (exit code 1 on any failure).
//
// Kept outside `bun test` (no `.test.ts` name): it needs Chromium and takes ~30 s.

import { mkdirSync } from 'node:fs';
import { join } from 'node:path';
import type { BrowserContext, Page } from 'playwright-core';
import { launch, phone } from '../test/smoke/browser.ts';
import { startSmoke } from '../test/smoke/server.ts';

const OUT = process.env['E2E_OUT'] ?? '/tmp/pwa-smoke/e2e';
const TIMEOUT = 15_000;
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
): Promise<void> {
  const context = await phone(browser, blockWorkers);
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
