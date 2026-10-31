// Smoke-only boot script (served by server.ts as /__smoke/boot.js, never part of dist/): prepares
// the browser for one smoke route, then loads the real app bundle.
//
// - Seeds the pairing in IndexedDB (the harness's fixed device key), unless the hash asks for the
//   pairing screen (`#route=pair`, or a `#pair=` invite link): then it clears it.
// - `theme=<id>` is stored as the remembered theme.
// - `gw=v1` selects the gateway that announces no `s4` (a cookie the WebSocket upgrade carries).
// The remaining hash (`route=…`, `s=…`, `pair=…`) is left for the app.

import { KeyStore } from '../../src/app/keystore.ts';
import { isThemeId, saveTheme } from '../../src/app/theme.ts';
import { fromBase64Url } from '../../src/remote/index.ts';

interface Seed {
  main: string;
  pairing: { secret: string; gatewayPublic: string; url: string; room: string; token: string; device: string; gateway: string; name: string };
}

function bytes(text: string): Uint8Array {
  const out = fromBase64Url(text);
  if (out === null) throw new Error('bad seed');
  return out;
}

async function boot(): Promise<void> {
  const params = new URLSearchParams(location.hash.slice(1));
  const seed = (await (await fetch('/__smoke/seed.json')).json()) as Seed;

  const theme = params.get('theme');
  if (isThemeId(theme)) saveTheme(localStorage, theme);
  document.cookie = `smoke_gw=${params.get('gw') === 'v1' ? 'v1' : 's4'}; path=/; SameSite=Strict`;

  const store = await KeyStore.open();
  if (params.get('route') === 'pair' || params.has('pair')) {
    await store.clear();
    if (params.get('route') === 'pair') history.replaceState(null, '', location.pathname);
  } else {
    const now = Date.now();
    const p = seed.pairing;
    await store.save({
      secret: bytes(p.secret),
      gatewayPublic: bytes(p.gatewayPublic),
      url: p.url,
      room: bytes(p.room),
      token: bytes(p.token),
      device: p.device,
      gateway: p.gateway,
      name: p.name,
      last: now - 1,
      pairedUnixMs: now - 3 * 24 * 3_600_000,
    });
  }
  store.close();
  // The app bundle's content-hashed name is only known to the server at run time.
  await import(seed.main);
}

void boot();
