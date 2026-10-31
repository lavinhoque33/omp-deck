// Starts the smoke harness and keeps it running: `bun test/smoke/serve.ts [--port N] [--minify]`.
// Prints the URL of every named route (README.md). Rebuilds only on restart.

import { ROUTES } from './routes.ts';
import { startSmoke } from './server.ts';

const args = process.argv.slice(2);
const portArg = args.indexOf('--port');
const port = portArg === -1 ? 4780 : Number(args[portArg + 1]);

const smoke = await startSmoke({ port, minify: args.includes('--minify'), log: (line) => console.log(`[smoke] ${line}`) });
console.log(`OmpDeck smoke harness on ${smoke.origin}`);
console.log(`pairing invite: ${smoke.url(`pair=${smoke.invite}`)}`);
for (const [name, hash] of Object.entries(ROUTES)) console.log(`${name.padEnd(24)} ${smoke.url(`${hash}&theme=aurora`)}`);
console.log('Add &theme=<aurora|blueprint|signal|margin|tidal|field> to any route; &gw=v1 for an old desktop. Ctrl+C stops.');

process.on('SIGINT', () => {
  void smoke.stop().then(() => process.exit(0));
});
