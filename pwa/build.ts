// Builds the PWA into dist/ (the relay serves it, see docs/notes/mobile-pwa.md):
// - the app from index.html, with content-hashed names under dist/assets/ (served `immutable`);
// - src/sw.ts → dist/sw.js, unhashed, with the precache list and a version injected;
// - public/ (manifest, icons) copied to the root;
// - `.gz`/`.br` siblings for compressible files; never a source map.
// `bun run build.ts --dev` builds unminified and serves dist/ on 127.0.0.1:5173, rebuilding on change.

import { createHash } from 'node:crypto';
import { cpSync, existsSync, mkdirSync, readdirSync, readFileSync, rmSync, statSync, watch, writeFileSync } from 'node:fs';
import { basename, dirname, extname, join, relative, resolve } from 'node:path';
import { brotliCompressSync, constants as zlib } from 'node:zlib';

const ROOT = import.meta.dir;
const COMPRESSIBLE: Record<string, true> = {
  '.html': true,
  '.js': true,
  '.css': true,
  '.webmanifest': true,
  '.json': true,
  '.svg': true,
  '.ttf': true,
  '.txt': true,
};
/** Linked from index.html but served from public/ at the root (unhashed). */
const HEAD_LINKS = [
  '<link rel="manifest" href="/manifest.webmanifest" />',
  '<link rel="icon" href="/icon-192.png" sizes="192x192" type="image/png" />',
  '<link rel="apple-touch-icon" href="/apple-touch-icon.png" sizes="180x180" />',
];

export interface BuildResult {
  outdir: string;
  /** URL paths the service worker precaches. */
  precache: string[];
  version: string;
}

function hash(bytes: Uint8Array): string {
  return createHash('sha256').update(bytes).digest('hex').slice(0, 10);
}

function files(dir: string): string[] {
  const out: string[] = [];
  for (const name of readdirSync(dir)) {
    const path = join(dir, name);
    if (statSync(path).isDirectory()) out.push(...files(path));
    else out.push(path);
  }
  return out;
}

/** Fonts go to dist/assets/ under a content hash and stay external (CSS would inline them as `data:`, which the CSP's `font-src 'self'` refuses). */
function fontPlugin(outdir: string): Bun.BunPlugin {
  return {
    name: 'hashed-fonts',
    setup(build) {
      build.onResolve({ filter: /\.(ttf|woff2)$/ }, (args) => {
        const source = resolve(dirname(args.importer), args.path);
        const bytes = readFileSync(source);
        const ext = extname(source);
        const name = `${basename(source, ext)}-${hash(bytes)}${ext}`;
        mkdirSync(join(outdir, 'assets'), { recursive: true });
        writeFileSync(join(outdir, 'assets', name), bytes);
        return { path: `/assets/${name}`, external: true };
      });
    },
  };
}

function check(result: Bun.BuildOutput, what: string): void {
  if (result.success) return;
  for (const log of result.logs) console.error(log);
  throw new Error(`${what} build failed`);
}

export async function build({ outdir = join(ROOT, 'dist'), minify = true }: { outdir?: string; minify?: boolean } = {}): Promise<BuildResult> {
  rmSync(outdir, { recursive: true, force: true });
  mkdirSync(outdir, { recursive: true });

  const app = await Bun.build({
    entrypoints: [join(ROOT, 'index.html')],
    outdir,
    target: 'browser',
    minify,
    splitting: true,
    sourcemap: 'none',
    naming: { entry: '[name].[ext]', chunk: 'assets/[name]-[hash].[ext]', asset: 'assets/[name]-[hash].[ext]' },
    define: { 'process.env.NODE_ENV': JSON.stringify(minify ? 'production' : 'development') },
    plugins: [fontPlugin(outdir)],
  });
  check(app, 'app');

  // The head links Bun would try to bundle are added after the build.
  const indexPath = join(outdir, 'index.html');
  const index = readFileSync(indexPath, 'utf8');
  if (!index.includes('</head>')) throw new Error('dist/index.html has no </head>');
  writeFileSync(indexPath, index.replace('</head>', `${HEAD_LINKS.join('')}</head>`));

  cpSync(join(ROOT, 'public'), outdir, { recursive: true });

  const precache = [
    '/index.html',
    ...files(join(outdir, 'assets')).map((path) => `/${relative(outdir, path).split('\\').join('/')}`),
    '/manifest.webmanifest',
    '/badge-96.png',
    '/icon-192.png',
    '/icon-512.png',
    '/apple-touch-icon.png',
  ].sort();
  const version = hash(
    new TextEncoder().encode(precache.join('\n') + readFileSync(indexPath, 'utf8')),
  );

  const sw = await Bun.build({
    entrypoints: [join(ROOT, 'src/sw.ts')],
    outdir,
    target: 'browser',
    format: 'iife',
    minify,
    sourcemap: 'none',
    naming: { entry: 'sw.js' },
    define: { __PRECACHE__: JSON.stringify(precache), __VERSION__: JSON.stringify(version) },
  });
  check(sw, 'service worker');

  for (const path of files(outdir)) {
    if (path.endsWith('.map')) throw new Error(`a source map was emitted: ${path}`);
    if (COMPRESSIBLE[extname(path)] !== true) continue;
    const bytes = readFileSync(path);
    writeFileSync(`${path}.gz`, Bun.gzipSync(bytes, { level: 9 }));
    writeFileSync(
      `${path}.br`,
      brotliCompressSync(bytes, { params: { [zlib.BROTLI_PARAM_QUALITY]: 11, [zlib.BROTLI_PARAM_SIZE_HINT]: bytes.length } }),
    );
  }
  return { outdir, precache, version };
}

async function dev(): Promise<void> {
  const outdir = join(ROOT, 'dist');
  let pending: Promise<unknown> = build({ outdir, minify: false });
  await pending;
  let timer: ReturnType<typeof setTimeout> | undefined;
  for (const dir of ['src', 'public']) {
    watch(join(ROOT, dir), { recursive: true }, () => {
      clearTimeout(timer);
      timer = setTimeout(() => {
        pending = pending
          .then(() => build({ outdir, minify: false }))
          .then(
            () => console.log('rebuilt'),
            (error: unknown) => console.error(error),
          );
      }, 150);
    });
  }
  const server = Bun.serve({
    hostname: '127.0.0.1',
    port: 5173,
    async fetch(request) {
      await pending;
      const path = new URL(request.url).pathname;
      const file = resolve(outdir, `.${path === '/' ? '/index.html' : path}`);
      if (!file.startsWith(outdir) || !existsSync(file) || statSync(file).isDirectory()) {
        return new Response('not found', { status: 404 });
      }
      return new Response(Bun.file(file));
    },
  });
  console.log(`serving dist/ on http://${server.hostname}:${server.port}`);
}

if (import.meta.main) {
  if (process.argv.includes('--dev')) {
    await dev();
  } else {
    const result = await build();
    console.log(`built ${result.precache.length} precached files into ${relative(process.cwd(), result.outdir) || '.'} (version ${result.version})`);
  }
}
