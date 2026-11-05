import { describe, expect, test } from 'bun:test';
import { install, installHelp, promptInstall, watchInstall } from '../src/app/install.ts';

const UA = {
  chrome: 'Mozilla/5.0 (Linux; Android 14; SM-S911B) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0.0.0 Mobile Safari/537.36',
  opera: 'Mozilla/5.0 (Linux; Android 14; SM-S911B) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0.0.0 Mobile Safari/537.36 OPR/85.0.0.0',
  samsung: 'Mozilla/5.0 (Linux; Android 14; SM-S911B) AppleWebKit/537.36 (KHTML, like Gecko) SamsungBrowser/26.0 Chrome/122.0.0.0 Mobile Safari/537.36',
  edge: 'Mozilla/5.0 (Linux; Android 14; SM-S911B) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0.0.0 Mobile Safari/537.36 EdgA/130.0.0.0',
  firefox: 'Mozilla/5.0 (Android 14; Mobile; rv:132.0) Gecko/132.0 Firefox/132.0',
  ipadDesktop: 'Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.0 Safari/605.1.15',
};

describe('install help', () => {
  const help = (userAgent: string, maxTouchPoints = 5, secure = true) => installHelp({ userAgent, maxTouchPoints, secure });

  test('names the browser even though Chromium forks carry "Chrome/"', () => {
    expect(help(UA.opera)).toContain('Opera');
    expect(help(UA.samsung)).toContain('Add page to');
    expect(help(UA.edge)).toContain('Edge');
    expect(help(UA.firefox)).toContain('Firefox');
    expect(help(UA.chrome)).toContain('Install app');
  });

  test('an iPad with a desktop user agent gets the Safari steps, a Mac does not', () => {
    expect(help(UA.ipadDesktop, 5)).toContain('Share');
    expect(help(UA.ipadDesktop, 0)).not.toContain('Share');
  });

  test('an insecure origin explains https instead of menu steps', () => {
    expect(help(UA.opera, 5, false)).toContain('https://');
  });
});

describe('install prompt', () => {
  test('the kept event opens the dialog once; appinstalled marks the app installed', async () => {
    const target = new EventTarget();
    watchInstall(target);
    let prompts = 0;
    const offer = (outcome: 'accepted' | 'dismissed') => {
      const event = Object.assign(new Event('beforeinstallprompt', { cancelable: true }), {
        prompt: async () => {
          prompts += 1;
        },
        userChoice: Promise.resolve({ outcome }),
      });
      target.dispatchEvent(event);
      return event;
    };

    expect(offer('dismissed').defaultPrevented).toBe(true);
    expect(install.get()).toEqual({ canPrompt: true, prompting: false, installed: false });

    await promptInstall();
    expect(prompts).toBe(1);
    expect(install.get()).toEqual({ canPrompt: false, prompting: false, installed: false });
    await promptInstall();
    expect(prompts).toBe(1);

    offer('accepted');
    await promptInstall();
    expect(install.get()).toEqual({ canPrompt: false, prompting: false, installed: true });

    offer('dismissed');
    target.dispatchEvent(new Event('appinstalled'));
    expect(install.get()).toEqual({ canPrompt: false, prompting: false, installed: true });
  });
});
