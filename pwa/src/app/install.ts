// Home Screen install: Chromium browsers (Chrome, Edge, Opera, Samsung Internet) fire
// `beforeinstallprompt` once the app is installable; the event is kept so the in-app banner can open
// the browser's own install dialog. Elsewhere (Firefox, iOS Safari, or before the event fires) the
// banner tells the user where their browser's install entry is.

type Listener = () => void;

/** `BeforeInstallPromptEvent` (not in TypeScript's DOM lib). */
export interface InstallPromptEvent extends Event {
  prompt(): Promise<unknown>;
  readonly userChoice: Promise<{ outcome: 'accepted' | 'dismissed' }>;
}

export interface InstallState {
  /** The browser offered its install dialog (`beforeinstallprompt`). */
  canPrompt: boolean;
  /** The dialog is open. */
  prompting: boolean;
  /** The browser reported the install (`appinstalled`) or the user accepted the dialog. */
  installed: boolean;
}

let state: InstallState = { canPrompt: false, prompting: false, installed: false };
let deferred: InstallPromptEvent | null = null;
const listeners = new Set<Listener>();

function set(patch: Partial<InstallState>): void {
  state = { ...state, ...patch };
  for (const listener of listeners) listener();
}

export const install = {
  subscribe(listener: Listener): () => void {
    listeners.add(listener);
    return () => listeners.delete(listener);
  },
  get(): InstallState {
    return state;
  },
};

/** Starts listening; call before the app renders (the event may fire right after load). */
export function watchInstall(target: Pick<EventTarget, 'addEventListener'>): void {
  target.addEventListener('beforeinstallprompt', (event) => {
    // Keeps the event for the banner's button instead of the browser's own mini-infobar.
    event.preventDefault();
    deferred = event as InstallPromptEvent;
    set({ canPrompt: true, installed: false });
  });
  target.addEventListener('appinstalled', () => {
    deferred = null;
    set({ canPrompt: false, prompting: false, installed: true });
  });
}

/** Opens the browser's install dialog. A prompt event works once; the browser may offer a new one later. */
export async function promptInstall(): Promise<void> {
  const event = deferred;
  if (event === null || state.prompting) return;
  deferred = null;
  set({ prompting: true });
  try {
    await event.prompt();
    const choice = await event.userChoice;
    set({ canPrompt: false, prompting: false, installed: state.installed || choice.outcome === 'accepted' });
  } catch {
    set({ canPrompt: false, prompting: false });
  }
}

/** Where to install from when the browser offers no prompt (or why it cannot). */
export function installHelp(env: { userAgent: string; maxTouchPoints: number; secure: boolean }): string {
  if (!env.secure) return 'Installing needs this page opened over https://.';
  const ua = env.userAgent;
  if (/iPhone|iPad|iPod/.test(ua) || (/Macintosh/.test(ua) && env.maxTouchPoints > 1)) {
    return 'In Safari, tap Share (the square with an arrow), then Add to Home Screen.';
  }
  // Opera, Samsung Internet and Edge also carry "Chrome/" in their user agent: test them first.
  if (/OPR\/|Opera/.test(ua)) return 'Open Opera’s menu and choose Add to → Home screen.';
  if (/SamsungBrowser\//.test(ua)) return 'Open the menu (☰) and choose Add page to → Home screen.';
  if (/EdgA?\//.test(ua)) return 'Open Edge’s menu (…) and choose Add to phone or Install.';
  if (/Firefox\//.test(ua)) return 'Open Firefox’s menu (⋮) and choose Add app to Home screen.';
  if (/Android/.test(ua)) return 'Open the browser menu (⋮) and choose Install app or Add to Home screen.';
  return 'Use your browser’s Install app option (address bar or menu).';
}
