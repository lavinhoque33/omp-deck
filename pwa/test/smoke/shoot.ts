// Screenshots smoke routes at 390×844: starts the harness (a fresh build), opens each target in
// headless Chromium, runs its steps, and writes `<out>/<label>-<theme>.png`.
//
//   bun test/smoke/shoot.ts --out /tmp/pwa-smoke/Me [--themes aurora,blueprint] [--wait 1500] [--full] TARGET…
//
// TARGET is `[label::]route[@step|step…]`: `route` is a name from routes.ts or a raw hash
// (`route=tree&session=s-convo`). Steps run in order after the screen settled:
//   click=<css>  tap=<visible text>  fill=<css>=<text>  files=<css>=<path>  press=<key>
//   wait=<ms>    scroll=<css>        top                  (scroll the window to the top)
// `label` defaults to the route name (or `shot-N` for a raw hash).

import { mkdirSync } from 'node:fs';
import { join } from 'node:path';
import type { Page } from 'playwright-core';
import { launch, phone } from './browser.ts';
import { ROUTES } from './routes.ts';
import { startSmoke } from './server.ts';

const args = process.argv.slice(2);
const option = (name: string): string | null => {
  const index = args.indexOf(name);
  if (index === -1) return null;
  const value = args[index + 1] ?? null;
  args.splice(index, 2);
  return value;
};
const out = option('--out') ?? '/tmp/pwa-smoke';
const themes = (option('--themes') ?? 'aurora,blueprint').split(',');
const settle = Number(option('--wait') ?? '1500');
const fullIndex = args.indexOf('--full');
const full = fullIndex !== -1;
if (full) args.splice(fullIndex, 1);
if (args.length === 0) {
  console.error('usage: bun test/smoke/shoot.ts --out DIR [--themes a,b] [--wait ms] [--full] TARGET…');
  console.error(`routes: ${Object.keys(ROUTES).join(' ')}`);
  process.exit(2);
}

async function step(page: Page, spec: string): Promise<void> {
  const eq = spec.indexOf('=');
  const verb = eq === -1 ? spec : spec.slice(0, eq);
  const rest = eq === -1 ? '' : spec.slice(eq + 1);
  const split = (): [string, string] => {
    const at = rest.lastIndexOf('=');
    return [rest.slice(0, at), rest.slice(at + 1)];
  };
  switch (verb) {
    case 'click':
      await page.locator(rest).first().click();
      break;
    case 'tap':
      await page.getByText(rest, { exact: false }).first().click();
      break;
    case 'fill': {
      const [selector, text] = split();
      await page.locator(selector).first().fill(text);
      break;
    }
    case 'files': {
      const [selector, path] = split();
      await page.locator(selector).first().setInputFiles(path);
      break;
    }
    case 'press':
      await page.keyboard.press(rest);
      break;
    case 'wait':
      await page.waitForTimeout(Number(rest));
      break;
    case 'scroll':
      await page.locator(rest).first().scrollIntoViewIfNeeded();
      break;
    case 'top':
      await page.evaluate(() => window.scrollTo(0, 0));
      break;
    default:
      throw new Error(`unknown step ${spec}`);
  }
  await page.waitForTimeout(250);
}

mkdirSync(out, { recursive: true });
const smoke = await startSmoke();
const browser = await launch();
let failed = false;
try {
  for (const [index, target] of args.entries()) {
    const labelled = target.split('::');
    const body = labelled.length > 1 ? (labelled[1] ?? '') : target;
    const [route = '', ...steps] = body.split('@').flatMap((part, i) => (i === 0 ? [part] : part.split('|')));
    const hash = ROUTES[route] ?? route;
    const label = labelled.length > 1 ? (labelled[0] ?? `shot-${index}`) : ROUTES[route] !== undefined ? route : `shot-${index}`;
    for (const theme of themes) {
      const context = await phone(browser);
      const page = await context.newPage();
      const errors: string[] = [];
      page.on('pageerror', (error) => errors.push(error.message));
      page.on('console', (message) => {
        if (message.type() === 'error') errors.push(message.text());
      });
      try {
        await page.goto(smoke.url(`${hash}&theme=${theme}`));
        await page.waitForSelector('#root > *');
        await page.waitForTimeout(settle);
        for (const spec of steps) await step(page, spec);
        const file = join(out, `${label}-${theme}.png`);
        await page.screenshot({ path: file, fullPage: full });
        console.log(`${file}${errors.length > 0 ? `  (console: ${errors.join(' | ')})` : ''}`);
      } catch (error) {
        failed = true;
        console.error(`${label}-${theme}: ${error instanceof Error ? error.message : String(error)}`);
      } finally {
        await context.close();
      }
    }
  }
} finally {
  await browser.close();
  await smoke.stop();
}
process.exit(failed ? 1 : 0);
