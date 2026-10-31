import { describe, expect, test } from 'bun:test';
import { routeFromHash } from '../src/app/nav.ts';
import { ROUTES } from './smoke/routes.ts';

describe('routeFromHash', () => {
  test('the notification deep link opens the conversation', () => {
    expect(routeFromHash('#s=s-convo')).toEqual({
      tab: 'sessions',
      stack: [{ kind: 'conversation', session: 's-convo', liveKey: null, title: 'Session' }],
    });
  });

  test('tabs and settings sections', () => {
    expect(routeFromHash('#route=inbox')).toEqual({ tab: 'inbox', stack: [] });
    expect(routeFromHash('#route=settings&section=usage')).toEqual({ tab: 'settings', stack: [{ kind: 'settings', section: 'usage' }] });
    expect(routeFromHash('#route=settings&section=nope')).toBeNull();
  });

  test('screens with their parameters', () => {
    expect(routeFromHash('#route=tree&session=s-1')?.stack).toEqual([{ kind: 'tree', session: 's-1', title: 'Session' }]);
    expect(routeFromHash('#route=worktrees&project=p-1')?.stack).toEqual([{ kind: 'worktrees', project: 'p-1', display: '' }]);
    expect(routeFromHash('#route=terminal&live=tmux%3A8%3Aomp-deck%3Ax')?.stack).toEqual([{ kind: 'terminal', liveKey: 'tmux:8:omp-deck:x', title: 'Terminal' }]);
    expect(routeFromHash('#route=subagent&session=s&agent=A')).toEqual({ tab: 'agents', stack: [{ kind: 'subagent', session: 's', agent: 'A' }] });
    expect(routeFromHash('#route=running')?.stack).toEqual([{ kind: 'running' }]);
    expect(routeFromHash('#route=browse')?.stack).toEqual([{ kind: 'browse' }]);
  });

  test('a link never names a title or folder (they are looked up by id)', () => {
    expect(routeFromHash('#route=worktrees&project=p-1&display=~%2Fscratch')?.stack).toEqual([{ kind: 'worktrees', project: 'p-1', display: '' }]);
    expect(routeFromHash('#route=changes&project=p-1&display=~%2Fscratch')?.stack).toEqual([{ kind: 'changes', project: 'p-1', display: '' }]);
    expect(routeFromHash('#route=tree&session=s-1&title=Spoofed')?.stack).toEqual([{ kind: 'tree', session: 's-1', title: 'Session' }]);
    expect(routeFromHash('#route=conversation&session=s-1&title=Spoofed')?.stack).toEqual([
      { kind: 'conversation', session: 's-1', liveKey: null, title: 'Session' },
    ]);
    expect(routeFromHash('#route=terminal&live=k&title=Spoofed')?.stack).toEqual([{ kind: 'terminal', liveKey: 'k', title: 'Terminal' }]);
  });

  test("a notification for a session without an id yet opens its conversation by live key (the gateway's encoding)", () => {
    expect(routeFromHash('#route=conversation&live=tmux%3A24%3Aomp-deck%3Aomp-new-17%26x%23y')).toEqual({
      tab: 'sessions',
      stack: [{ kind: 'conversation', session: null, liveKey: 'tmux:24:omp-deck:omp-new-17&x#y', title: 'Session' }],
    });
  });

  test('missing parameters, unknown routes and no route are null', () => {
    expect(routeFromHash('#route=tree')).toBeNull();
    expect(routeFromHash('#route=changes')).toBeNull();
    expect(routeFromHash('#route=bogus')).toBeNull();
    expect(routeFromHash('#theme=aurora')).toBeNull();
    expect(routeFromHash('')).toBeNull();
  });

  test('every documented smoke route but pair parses', () => {
    for (const [name, hash] of Object.entries(ROUTES)) {
      if (name === 'pair') continue;
      expect([name, routeFromHash(`#${hash}`)]).not.toEqual([name, null]);
    }
  });
});
