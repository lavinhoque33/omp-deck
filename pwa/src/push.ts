// The gateway's declarative Web Push payload (`{web_push, notification: {...}}`), as the service
// worker shows it.

/** Cache Storage where the app keeps the VAPID key it subscribed with (the service worker's activate keeps it). */
export const PUSH_STATE_CACHE = 'push-state';
/** The key's entry in {@link PUSH_STATE_CACHE} (raw key bytes). */
export const PUSH_KEY_URL = '/__push-key';
/** Posted by the service worker to open windows after it renewed the subscription. */
export const PUSH_CHANGED_MESSAGE = 'push_subscription_changed';

export interface PushPayload {
  title: string;
  body: string;
  /** A same-origin path and hash (anything else becomes `/`). */
  navigate: string;
  tag: string | null;
  silent: boolean;
}

/** Shown when a push carries no readable payload: every push must show something. */
export const GENERIC_PUSH: PushPayload = {
  title: 'OmpDeck',
  body: 'Something on your desktop needs a look.',
  navigate: '/',
  tag: null,
  silent: false,
};

/**
 * The in-app destination of a notification's `navigate` (the gateway sends an absolute URL on this
 * origin): path and hash only, never another origin. Anything else is `/`.
 */
export function safeNavigate(navigate: unknown, origin: string): string {
  if (typeof navigate !== 'string') return '/';
  let url: URL;
  try {
    url = new URL(navigate, origin);
  } catch {
    return '/';
  }
  return url.origin === origin ? `${url.pathname}${url.hash}` : '/';
}

/** Reads a push payload; null when it is malformed. */
export function parsePush(text: string, origin: string): PushPayload | null {
  let value: unknown;
  try {
    value = JSON.parse(text);
  } catch {
    return null;
  }
  if (typeof value !== 'object' || value === null || !('notification' in value)) return null;
  const notification: unknown = value.notification;
  if (typeof notification !== 'object' || notification === null) return null;
  const title = 'title' in notification ? notification.title : undefined;
  const body = 'body' in notification ? notification.body : undefined;
  const navigate = 'navigate' in notification ? notification.navigate : undefined;
  const tag = 'tag' in notification ? notification.tag : undefined;
  const silent = 'silent' in notification ? notification.silent : undefined;
  if (typeof title !== 'string' || title.length === 0) return null;
  return {
    title,
    body: typeof body === 'string' ? body : '',
    navigate: safeNavigate(navigate, origin),
    tag: typeof tag === 'string' ? tag : null,
    silent: silent === true,
  };
}
