// Headless Chromium for the smoke screenshots and the e2e run (playwright-core drives an
// installed browser; nothing is downloaded).

import { existsSync } from 'node:fs';
import { homedir } from 'node:os';
import { join } from 'node:path';
import { type Browser, type BrowserContext, chromium } from 'playwright-core';

const CANDIDATES = [
  process.env['OMPDECK_CHROME'],
  join(homedir(), '.cache/ms-playwright/chromium-1248/chrome-linux64/chrome'),
  '/usr/bin/google-chrome',
  '/usr/bin/chromium',
];

export function chromePath(): string {
  const found = CANDIDATES.find((path): path is string => path !== undefined && existsSync(path));
  if (found === undefined) throw new Error(`no Chromium found (tried ${CANDIDATES.filter(Boolean).join(', ')}); set OMPDECK_CHROME`);
  return found;
}

export function launch(): Promise<Browser> {
  return chromium.launch({ executablePath: chromePath(), headless: true, args: ['--no-sandbox', '--disable-dev-shm-usage'] });
}

const PHONE = {
  viewport: { width: 390, height: 844 },
  deviceScaleFactor: 2,
  isMobile: true,
  hasTouch: true,
  userAgent:
    'Mozilla/5.0 (Linux; Android 15; Pixel 9) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/141.0.0.0 Mobile Safari/537.36',
  acceptDownloads: true,
} as const;

/**
 * A phone-sized context (390×844 CSS px at 2×); every context starts with a fresh profile.
 * `blockWorkers` emulates a browser without service workers.
 */
export function phone(browser: Browser, blockWorkers = false): Promise<BrowserContext> {
  return browser.newContext({ ...PHONE, serviceWorkers: blockWorkers ? 'block' : 'allow' });
}

/**
 * A phone-sized browser on a real (non-incognito) profile in `userDataDir`: the Push API exists
 * there, as on a phone (Chrome disables it in incognito contexts, and says so on the console).
 */
export function persistentPhone(userDataDir: string): Promise<BrowserContext> {
  return chromium.launchPersistentContext(userDataDir, {
    ...PHONE,
    executablePath: chromePath(),
    headless: true,
    args: ['--no-sandbox', '--disable-dev-shm-usage'],
    serviceWorkers: 'allow',
  });
}
