import { afterEach, describe, expect, test } from 'bun:test';
import { detectSupport, dropBrowserPush, keyChanged, PushController, type PushEnv, subscriptionOp } from '../src/app/push-client.ts';
import { type api, fromBase64Url, toBase64Url } from '../src/remote/index.ts';

const KEY_A = toBase64Url(Uint8Array.from({ length: 65 }, (_, i) => (i === 0 ? 4 : i)));
const KEY_B = toBase64Url(Uint8Array.from({ length: 65 }, (_, i) => (i === 0 ? 4 : 200 - i)));

function welcome(patch: Partial<api.Welcome> = {}): api.Welcome {
  return {
    protocol: 1,
    gateway_version: '0.10.0',
    epoch: '00112233445566778899aabbccddeeff',
    device: 'd-0123456789abcdef',
    name: 'Pixel',
    permissions: { control: true, manage: true },
    next_seq: 1,
    limits: { max_message: 1 << 20, window_bytes: 1 << 20, window_messages: 32, max_inflight: 32, max_views: 4, max_feeds: 16 },
    features: ['s4'],
    push_key: KEY_A,
    ...patch,
  };
}

class FakeSubscription {
  unsubscribed = false;
  constructor(
    readonly endpoint: string,
    readonly key: string,
  ) {}
  get options(): PushSubscriptionOptions {
    return { userVisibleOnly: true, applicationServerKey: (fromBase64Url(this.key) as Uint8Array<ArrayBuffer>).buffer };
  }
  getKey(name: PushEncryptionKeyName): ArrayBuffer {
    return Uint8Array.from(name === 'p256dh' ? [1, 2, 3] : [9, 8]).buffer;
  }
  async unsubscribe(): Promise<boolean> {
    this.unsubscribed = true;
    return true;
  }
}

class FakeBrowser {
  permission: NotificationPermission = 'default';
  answer: NotificationPermission = 'granted';
  current: FakeSubscription | null = null;
  subscribed: string[] = [];
  remembered: string[] = [];
  asks = 0;
  readonly env: PushEnv = {
    support: 'supported',
    permission: () => this.permission,
    requestPermission: async () => {
      this.asks++;
      this.permission = this.answer;
      return this.answer;
    },
    registration: async () =>
      ({
        pushManager: {
          getSubscription: async () => this.current,
          subscribe: async (options: PushSubscriptionOptionsInit) => {
            const key = toBase64Url(new Uint8Array(options.applicationServerKey as Uint8Array<ArrayBuffer>));
            this.subscribed.push(key);
            this.current = new FakeSubscription(`https://push.example/${this.subscribed.length}`, key);
            return this.current;
          },
        },
      }) as unknown as ServiceWorkerRegistration,
    rememberKey: async (key) => {
      this.remembered.push(toBase64Url(key));
    },
  };
}

class FakeClient {
  readonly sent: api.Op[] = [];
  constructor(public welcome: api.Welcome | null) {}
  getState = () => ({ welcome: this.welcome });
  request = async (op: api.Op): Promise<api.Resp> => {
    this.sent.push(op);
    return { type: 'unit' };
  };
}

describe('support', () => {
  const base = { userAgent: 'Mozilla/5.0 (Linux; Android 15)', maxTouchPoints: 5, standalone: false, hasServiceWorker: true, hasPushManager: true, hasNotification: true };
  test('Android and desktop browsers with the APIs are supported', () => {
    expect(detectSupport(base)).toBe('supported');
  });
  test('iOS outside the Home Screen app needs the Home Screen hint', () => {
    expect(detectSupport({ ...base, userAgent: 'Mozilla/5.0 (iPhone; CPU iPhone OS 18_4 like Mac OS X)', hasPushManager: false })).toBe('ios_home_screen');
    expect(detectSupport({ ...base, userAgent: 'Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7)', maxTouchPoints: 5 })).toBe('ios_home_screen');
    expect(detectSupport({ ...base, userAgent: 'Mozilla/5.0 (iPhone; CPU iPhone OS 18_4 like Mac OS X)', standalone: true })).toBe('supported');
  });
  test('missing APIs are unsupported', () => {
    expect(detectSupport({ ...base, hasPushManager: false })).toBe('unsupported');
  });
});

describe('subscription helpers', () => {
  test('push_subscribe carries the endpoint and base64url keys', () => {
    expect(subscriptionOp(new FakeSubscription('https://push.example/x', KEY_A))).toEqual({ op: 'push_subscribe', endpoint: 'https://push.example/x', p256dh: 'AQID', auth: 'CQg' });
  });
  test('keyChanged compares the subscription key with push_key', () => {
    const subscription = new FakeSubscription('e', KEY_A);
    expect(keyChanged(subscription, KEY_A)).toBe(false);
    expect(keyChanged(subscription, KEY_B)).toBe(true);
  });
});

describe('PushController', () => {
  test('enable asks permission, subscribes with push_key, registers it and remembers the key', async () => {
    const browser = new FakeBrowser();
    const client = new FakeClient(welcome());
    const push = new PushController(client, browser.env);
    await push.enable();
    expect(browser.asks).toBe(1);
    expect(browser.subscribed).toEqual([KEY_A]);
    expect(browser.remembered).toEqual([KEY_A]);
    expect(client.sent).toEqual([{ op: 'push_subscribe', endpoint: 'https://push.example/1', p256dh: 'AQID', auth: 'CQg' }]);
    expect(push.getState()).toMatchObject({ subscribed: true, busy: false, error: null, permission: 'granted' });
  });

  test('a denied permission explains and sends nothing', async () => {
    const browser = new FakeBrowser();
    browser.answer = 'denied';
    const client = new FakeClient(welcome());
    const push = new PushController(client, browser.env);
    await push.enable();
    expect(client.sent).toEqual([]);
    expect(push.getState().error).toContain('blocked');
  });

  test('an older desktop or iOS outside the Home Screen never prompts', async () => {
    const browser = new FakeBrowser();
    const old = new PushController(new FakeClient(welcome({ features: [] })), browser.env);
    await old.enable();
    expect(old.getState().error).toContain('newer OmpDeck');
    const ios = new PushController(new FakeClient(welcome()), { ...browser.env, support: 'ios_home_screen' });
    await ios.enable();
    expect(ios.getState().error).toContain('Home Screen');
    expect(browser.asks).toBe(1);
  });

  test('sync at start re-sends an existing subscription without prompting', async () => {
    const browser = new FakeBrowser();
    browser.permission = 'granted';
    browser.current = new FakeSubscription('https://push.example/old', KEY_A);
    const client = new FakeClient(welcome());
    await new PushController(client, browser.env).sync();
    expect(browser.asks).toBe(0);
    expect(browser.subscribed).toEqual([]);
    expect(client.sent).toEqual([{ op: 'push_subscribe', endpoint: 'https://push.example/old', p256dh: 'AQID', auth: 'CQg' }]);
  });

  test('sync resubscribes when push_key changed', async () => {
    const browser = new FakeBrowser();
    browser.permission = 'granted';
    const old = new FakeSubscription('https://push.example/old', KEY_A);
    browser.current = old;
    const client = new FakeClient(welcome({ push_key: KEY_B }));
    await new PushController(client, browser.env).sync();
    expect(old.unsubscribed).toBe(true);
    expect(browser.subscribed).toEqual([KEY_B]);
    expect(client.sent).toEqual([{ op: 'push_subscribe', endpoint: 'https://push.example/1', p256dh: 'AQID', auth: 'CQg' }]);
  });

  test('sync does nothing without a subscription, a push key or s4', async () => {
    const browser = new FakeBrowser();
    browser.permission = 'granted';
    const client = new FakeClient(welcome());
    await new PushController(client, browser.env).sync();
    browser.current = new FakeSubscription('e', KEY_A);
    await new PushController(new FakeClient(welcome({ features: [] })), browser.env).sync();
    const noKey = welcome();
    delete noKey.push_key;
    await new PushController(new FakeClient(noKey), browser.env).sync();
    expect(client.sent).toEqual([]);
  });

  test('disable unregisters at the desktop and drops the browser subscription; prefs are sent', async () => {
    const browser = new FakeBrowser();
    const subscription = new FakeSubscription('e', KEY_A);
    browser.current = subscription;
    const client = new FakeClient(welcome());
    const push = new PushController(client, browser.env);
    await push.disable();
    const prefs = { turn_finished: true, attention: false, ended: true, show_titles: false };
    await push.setPrefs(prefs);
    expect(client.sent).toEqual([{ op: 'push_unsubscribe' }, { op: 'push_prefs', prefs }]);
    expect(subscription.unsubscribed).toBe(true);
    expect(push.getState().subscribed).toBe(false);
  });
});

describe('dropBrowserPush', () => {
  const saved = { sw: Object.getOwnPropertyDescriptor(navigator, 'serviceWorker'), caches: Object.getOwnPropertyDescriptor(globalThis, 'caches') };
  afterEach(() => {
    if (saved.sw) Object.defineProperty(navigator, 'serviceWorker', saved.sw);
    else Reflect.deleteProperty(navigator, 'serviceWorker');
    if (saved.caches) Object.defineProperty(globalThis, 'caches', saved.caches);
    else Reflect.deleteProperty(globalThis, 'caches');
  });

  function install(unsubscribe: () => Promise<boolean>): string[] {
    const deleted: string[] = [];
    Object.defineProperty(navigator, 'serviceWorker', {
      configurable: true,
      value: { getRegistration: async () => ({ pushManager: { getSubscription: async () => ({ unsubscribe }) } }) },
    });
    Object.defineProperty(globalThis, 'caches', {
      configurable: true,
      value: { open: async (name: string) => ({ delete: async (url: string) => deleted.push(`${name}:${url}`) > 0 }) },
    });
    return deleted;
  }

  test('unsubscribes and deletes the kept key', async () => {
    const deleted = install(async () => true);
    await dropBrowserPush();
    expect(deleted).toHaveLength(1);
  });

  test('deletes the kept key even when unsubscribe throws, and reports the failure', async () => {
    const deleted = install(async () => {
      throw new Error('push service unreachable');
    });
    await expect(dropBrowserPush()).rejects.toThrow('push service unreachable');
    expect(deleted).toHaveLength(1);
  });
});
