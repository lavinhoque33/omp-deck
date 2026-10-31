import { afterAll, beforeAll, describe, expect, test } from 'bun:test';
import { existsSync, mkdtempSync, readFileSync, readdirSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { type BuildResult, build } from '../build.ts';

let outdir: string;
let result: BuildResult;

beforeAll(async () => {
  outdir = mkdtempSync(join(tmpdir(), 'ompdeck-pwa-'));
  result = await build({ outdir });
}, 120_000);

afterAll(() => {
  rmSync(outdir, { recursive: true, force: true });
});

describe('build', () => {
  test('index.html loads hashed assets and links the manifest and icons', () => {
    const html = readFileSync(join(outdir, 'index.html'), 'utf8');
    const scripts = [...html.matchAll(/src="\.?\/?(assets\/[^"]+\.js)"/g)].map((match) => match[1]);
    const styles = [...html.matchAll(/href="\.?\/?(assets\/[^"]+\.css)"/g)].map((match) => match[1]);
    expect(scripts.length).toBeGreaterThan(0);
    expect(styles.length).toBeGreaterThan(0);
    for (const path of [...scripts, ...styles]) {
      expect(path).toMatch(/-[a-z0-9]{6,}\.(js|css)$/);
      expect(existsSync(join(outdir, path ?? ''))).toBe(true);
    }
    expect(html).toContain('href="/manifest.webmanifest"');
    expect(html).toContain('href="/apple-touch-icon.png"');
    expect(html).not.toMatch(/<script>(?!<)/);
  });

  test('every file under assets/ is content-hashed', () => {
    for (const name of readdirSync(join(outdir, 'assets'))) {
      expect(name).toMatch(/-[a-z0-9]{6,}\.[a-z0-9]+(\.(gz|br))?$/);
    }
  });

  test('fonts are separate files, never inlined (CSP font-src self)', () => {
    const css = readdirSync(join(outdir, 'assets')).filter((name) => name.endsWith('.css'));
    for (const name of css) expect(readFileSync(join(outdir, 'assets', name), 'utf8')).not.toContain('data:font');
    expect(readdirSync(join(outdir, 'assets')).filter((name) => name.endsWith('.ttf'))).toHaveLength(5);
  });

  test('sw.js carries the precache list and version', () => {
    const sw = readFileSync(join(outdir, 'sw.js'), 'utf8');
    expect(result.precache).toContain('/index.html');
    expect(result.precache).toContain('/manifest.webmanifest');
    for (const path of result.precache) {
      expect(sw).toContain(path);
      expect(existsSync(join(outdir, path))).toBe(true);
    }
    expect(result.precache.some((path) => path.endsWith('.gz') || path.endsWith('.br'))).toBe(false);
    expect(sw).toContain(result.version);
    expect(sw).not.toMatch(/^\s*(import|export)\s/m);
  });

  test('the manifest is installable', () => {
    const manifest = JSON.parse(readFileSync(join(outdir, 'manifest.webmanifest'), 'utf8')) as {
      id: string;
      scope: string;
      start_url: string;
      display: string;
      icons: { src: string; sizes: string; purpose: string }[];
    };
    expect(manifest).toMatchObject({ id: '/', scope: '/', start_url: '/?source=pwa', display: 'standalone' });
    expect(manifest.icons.map((icon) => `${icon.sizes}:${icon.purpose}`).sort()).toEqual([
      '192x192:any',
      '512x512:any',
      '512x512:maskable',
    ]);
    for (const icon of manifest.icons) expect(existsSync(join(outdir, icon.src))).toBe(true);
    expect(existsSync(join(outdir, 'apple-touch-icon.png'))).toBe(true);
  });

  test('compressed siblings exist and no source map is shipped', () => {
    const all = (dir: string): string[] =>
      readdirSync(dir, { withFileTypes: true }).flatMap((entry) =>
        entry.isDirectory() ? all(join(dir, entry.name)) : [join(dir, entry.name)],
      );
    const files = all(outdir);
    expect(files.some((path) => path.endsWith('.map'))).toBe(false);
    expect(files).toContain(join(outdir, 'index.html.br'));
    expect(files).toContain(join(outdir, 'index.html.gz'));
    expect(files).toContain(join(outdir, 'sw.js.br'));
    for (const path of files) expect(path).not.toMatch(/\/\.[^/]+$/);
  });
});

describe('test-only code stays out of dist', () => {
  test('no smoke harness, boot script or gateway fixtures ship', () => {
    const all = (dir: string): string[] =>
      readdirSync(dir, { withFileTypes: true }).flatMap((entry) =>
        entry.isDirectory() ? all(join(dir, entry.name)) : [join(dir, entry.name)],
      );
    const markers = ['__smoke', 'smoke_gw', 'SmokeGateway', 'ompdeck-smoke', 'playwright'];
    for (const path of all(outdir)) {
      if (/\.(gz|br|png|ttf)$/.test(path)) continue;
      const text = readFileSync(path, 'utf8');
      for (const marker of markers) expect([path, text.includes(marker)]).toEqual([path, false]);
    }
    const html = readFileSync(join(outdir, 'index.html'), 'utf8');
    expect(html).not.toContain('boot.js');
  });
});
