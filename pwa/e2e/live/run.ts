// `bun run live`: the phone app's live gate. Builds dist/, starts the live stack (the real relay
// serving dist/, a real gateway and real omp on a scripted model:
// crates/deck-service/tests/pwa_live_stack.rs), runs ./spec.ts against it with Chromium, and
// stops the stack. Exit code 1 on any failure; skipped (exit 0) when omp is not installed.

import { existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { runLive, type Stack } from './spec.ts';

const PWA = join(import.meta.dir, '../..');
const REPO = join(PWA, '..');
const CARGO_TEST = ['cargo', 'test', '-p', 'deck-service', '--test', 'pwa_live_stack'];
/** The launcher's pairing window and omp's first start, once the test binary is built. */
const STACK_UP = 120_000;

if (Bun.which('omp') === null) {
  console.log('live: skipped — omp is not installed (the live gate needs a real omp on PATH)');
  process.exit(0);
}

function step(cmd: string[], cwd: string): void {
  console.log(`live: ${cmd.join(' ')}`);
  const result = Bun.spawnSync(cmd, { cwd, stdout: 'inherit', stderr: 'inherit' });
  if (result.exitCode !== 0) {
    console.error(`live: \`${cmd.join(' ')}\` failed (${result.exitCode})`);
    process.exit(1);
  }
}

step(['bun', 'run', 'build'], PWA);
step([...CARGO_TEST, '--no-run'], REPO);

const dir = mkdtempSync(join(tmpdir(), 'pwa-live-'));
const urlFile = join(dir, 'stack.json');
const stopFile = join(dir, 'stop');
const log = join(dir, 'stack.log');
// A private session bus when available: the gateway's desktop notifications never reach the
// user's desktop.
const dbus = Bun.which('dbus-run-session');
const stack = Bun.spawn([...(dbus === null ? [] : [dbus, '--']), ...CARGO_TEST, '--', '--ignored', '--exact', '--nocapture', 'pwa_live_stack'], {
  cwd: REPO,
  env: { ...process.env, OMPDECK_LIVE_URL_FILE: urlFile, OMPDECK_LIVE_STOP_FILE: stopFile },
  stdout: Bun.file(log),
  stderr: Bun.file(`${log}.err`),
});
let exited = false;
void stack.exited.then(() => {
  exited = true;
});

function stackLog(): string {
  const read = (path: string) => (existsSync(path) ? readFileSync(path, 'utf8') : '');
  return `${read(log)}${read(`${log}.err`)}`.split('\n').slice(-120).join('\n');
}

let failed = false;
try {
  const end = Date.now() + STACK_UP;
  while (!existsSync(urlFile)) {
    if (exited || Date.now() > end) throw new Error(exited ? `the stack exited (${stack.exitCode})` : 'the stack did not come up in time');
    await Bun.sleep(200);
  }
  const info = JSON.parse(readFileSync(urlFile, 'utf8')) as Stack;
  console.log(`live: stack up — app ${info.origin}, control ${info.control}`);
  const started = performance.now();
  const run = await runLive(info);
  console.log(`live: all steps passed in ${Math.round((performance.now() - started) / 1000)} s`);
  console.log(`live: screenshots\n${run.shots.map((path) => `  ${path}`).join('\n')}`);
} catch (error) {
  failed = true;
  console.error(`live: FAILED — ${error instanceof Error ? (error.stack ?? error.message) : String(error)}`);
} finally {
  writeFileSync(stopFile, '');
  const stopped = await Promise.race([stack.exited.then(() => true), Bun.sleep(90_000).then(() => false)]);
  if (!stopped) {
    stack.kill('SIGKILL');
    failed = true;
    console.error('live: the stack did not stop in time');
  } else if (stack.exitCode !== 0) {
    failed = true;
    console.error(`live: the stack exited ${stack.exitCode}`);
  }
  if (failed) console.error(`---- stack log (tail) ----\n${stackLog()}\n---- end ----`);
  rmSync(dir, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
