// `bun run live`: the phone app's live gate. Builds dist/, starts the live stack (the real relay
// serving dist/, a real gateway and real omp on a scripted model:
// crates/deck-service/tests/pwa_live_stack.rs), runs ./spec.ts against it with Chromium, and
// stops the stack. Exit code 1 on any failure; skipped (exit 0) when omp is not installed.

import { runLive } from './spec.ts';
import { build, requireOmp, type Running, startStack } from './stack.ts';

requireOmp('live');
build('live');

let failed = false;
let stack: Running | null = null;
try {
  stack = await startStack();
  console.log(`live: stack up — app ${stack.info.origin}, control ${stack.info.control}`);
  const started = performance.now();
  const run = await runLive(stack.info);
  console.log(`live: all steps passed in ${Math.round((performance.now() - started) / 1000)} s`);
  console.log(`live: screenshots\n${run.shots.map((path) => `  ${path}`).join('\n')}`);
} catch (error) {
  failed = true;
  console.error(`live: FAILED — ${error instanceof Error ? (error.stack ?? error.message) : String(error)}`);
  if (stack !== null) console.error(`---- stack log (tail) ----\n${stack.log()}\n---- end ----`);
} finally {
  if (stack !== null && !(await stack.stop())) failed = true;
}
process.exit(failed ? 1 : 0);
