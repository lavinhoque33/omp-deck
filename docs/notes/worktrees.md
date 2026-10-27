# Git worktree sessions — facts and agreed design

Investigation 2026-10-23. omp facts are from the v18.12.6 source on GitHub (the local `~/PersProject/oh-my-pi`
checkout is v18.4.10, too old for `worktree.*` settings) unless marked "observed" (sandbox run with omp 18.12.6).

## omp facts

- `omp worktree [list|add|clear]` (alias `wt`; `commands/worktree.ts`, `cli/worktree-cli.ts`).
  - `add [path] [commit] [-C dir] [-b|-B branch] [-d] [-q]`: creates the worktree exactly at `path` (relative to
    `-C`/cwd); `-b` makes a new branch from `commit` (default `HEAD`); destination must be absent or empty; runs
    the `post-checkout` hook. No registry besides git's own `.git/worktrees/`.
  - Clone-first (`worktree.clone`, default true): copy-on-write clone of the checkout incl. gitignored files
    (`.env`, `target/`, `node_modules`; omp issue #15476), falling back to a plain checkout. **Observed:** this box
    (`~/PersProject` on ext4) always falls back — `warning: worktree clone fell back to plain checkout: … FICLONE
    unsupported` — so new worktrees have no ignored files.
  - `list --json` scans only the managed base's children (observed): `[{"path","kind":"pr-checkout|task-isolation|
    empty|stray","parentRepo"?,"branch"?,"orphanReason"?}]`; a worktree made by `add` under the base is listed as
    `pr-checkout` with `parentRepo` + `branch`; one made elsewhere is not listed.
  - `clear` removes only orphaned entries; `--all` force-removes everything incl. live task sandboxes and dirty
    trees — never call it from OmpDeck.
- Managed base: `OMP_WORKTREE_DIR` > `worktree.base` setting (absolute or `~`) > `~/.omp/wt` (`utils/src/dirs.ts`).
  omp's own names: `/wt` → `<slug(branch)>-<hash7(primaryRoot)>[-n]`; PR checkouts `<N>-<hash7>` on `pr-<N>`.
- `/wt [branch]` (alias `/worktree`; refused while streaming; works over RPC/ACP): branch default
  `wt/<yyyymmdd-hhmmss>`, refuses an existing branch or a repo without commits, carries staged/unstaged/untracked
  changes, then relocates the session like `/move` (`SessionManager.moveTo`): same session id, file + artifacts
  renamed into the worktree's project bucket, `header.cwd` rewritten, old path appended to
  `header.previousSessionFiles`, breadcrumb refreshed. `worktree.cleanSource` resets the source afterwards.
- `worktree.onStart` off|ask|create (fresh interactive launches only), `worktree.onExit` keep|ask|remove (only for
  worktrees created by that launch). omp has **no merge/PR/finish command** for worktrees.
- Session header has no worktree/repo field. omp recomputes siblings via git's worktree list
  (`session/session-paths.ts` `worktreeSessionDirs`). `--resume` of a session whose worktree was removed opens in
  the launch directory (`SessionManager.isFromRemovedWorktree` / `openRelocatedSession`).
- Primary repo of any worktree (observed): `git -C <wt> rev-parse --path-format=absolute --git-common-dir` →
  `<primary>/.git`; `<wt>/.git` is a file `gitdir: <primary>/.git/worktrees/<name>`.

## OmpDeck facts (current tree)

- Sidebar groups by exact header `cwd` (`deck-core/src/sessions.rs` `Scanner::scan`): every worktree is its own
  project group; a `/wt`-moved session leaves its repo's group. Folder scope is `cwd.starts_with(scope)`
  (`omp-deck/src/folder_scope.rs`), so worktrees outside the repo dir are hidden in per-folder mode.
- Status bar git info (`deck-core/src/status.rs` `git_info`, `git -C <cwd> status --porcelain=v2`) is already
  correct inside a worktree; the watcher follows the breadcrumb cwd after `/wt`/`/move`.
- `tab.cwd` is never updated after `/wt`/`/move` (only `session_path` follows via breadcrumb / `/proc` fds), so
  restore/reopen use the stale cwd.
- Drive project-sync rows and `AppConfig.folder_tabs` are keyed by cwd and are never pruned when a folder vanishes.
- Reusable: `usage::run_bounded` (tree kill, bounded pipes) for git/omp commands; `App::spawn_omp` workers;
  `confirm::show_dialog`; `transfer::FolderDialog` pattern; live-control `prompt` (ctl) and the safe-injection
  queue (idle gating) for sending `/wt` and prompts.

## Agreed design (user decisions 2026-10-23)

1. **New session in worktree** (project header menu + next to "+"): dialog with branch name, base ref (current
   `HEAD` or any local/remote branch) and location defaulting to the managed base as
   `<base>/<branch-slug>-<repo-name>[-n]` (base resolved like omp: `OMP_WORKTREE_DIR` > `worktree.base` > `~/.omp/wt`).
   Worker runs `omp worktree add -b <branch> <dir> <base-ref> -C <repo>` (omp creates the dir; OmpDeck still never
   writes `~/.omp/**` itself), then opens a tab running `omp` in the worktree. Clean checkout: no copying of
   ignored files.
2. **Move this session to a new worktree** (running tab): injects `/wt <branch>` when idle (carries uncommitted
   changes); the tab's cwd/session follow the relocation.
3. **Sidebar/status** (revised 2026-10-26, user: worktree sessions belong to the parent folder): a worktree's
   sessions are rows of their primary repo's group with a branch chip (also for `/wt` sessions and omp PR
   checkouts); no nested groups; per-folder scope includes the repo's worktrees; status bar shows
   "worktree of <repo>".
4. **Finish worktree**: the session's own agent decides — OmpDeck sends the worktree's omp session a finish prompt
   (commit, merge into `<base>` following the project's rules, otherwise ask the user in the chat, report the
   outcome). Requires a running session with live control; a closed session is resumed first.
5. **Auto-remove when safe** (only as part of Finish, once the agent's turn ended): branch merged into base
   (tip is an ancestor of base), worktree clean (no staged/unstaged/untracked), then OmpDeck moves the worktree's
   sessions into the primary with omp's `/move` (open tabs keep running there; tabs that do not move in 30 s are
   stopped), runs `git worktree remove` from the primary checkout and deletes the merged branch
   (`git branch -d`). Anything unsafe → keep it and say why; manual "Remove worktree…" stays available with the
   same checks.
6. **Housekeeping**: update `tab.cwd` from the breadcrumb on relocation; prune Sync rows / `folder_tabs` /
   recent folders for removed worktrees (Sync rows: offer, don't silently drop backups config).

## Relocation (verified 2026-10-26, omp 18.13.1, sandbox with temp HOME/PI_CODING_AGENT_DIR/OMP_WORKTREE_DIR)

- omp has no "stored as folder A, tools in folder B" mode: tool cwd = header `cwd` = process cwd. `--session-dir`
  (`PI_CODING_AGENT_SESSION_DIR`) only places the file (header cwd stays the launch dir).
- `/move <path>` (`slash-commands/builtin-lifecycle.ts:710-740`; args = rest of the line, resolved against the
  session cwd; must be an existing dir; refused while streaming): `SessionManager.moveTo` (`session-manager.ts:2488`)
  renames jsonl + artifacts into the target bucket, sets header `cwd`, appends `previousSessionFiles`, refreshes the
  breadcrumb; the TUI then `process.chdir`s (tools/settings/skills re-scoped). No confirm for an existing dir, no
  model call, nothing appended for the model; the old bucket dir is left empty. Observed: `✔ Moved to <dir>`.
- Headless: `omp --mode rpc --resume <abs path>` + `{"type":"prompt","message":"/move <dir>"}` →
  `{"success":true,"data":{"agentInvoked":false}}` + `command_output` `Moved to <dir>.`; works whether or not the
  worktree still exists.
- `--resume <path>` (contains `/` or ends `.jsonl`) never relocates (`main.ts:1295-1304`), even when the header cwd
  is gone: it opens from the launch dir and appends `session_exit`. Id-prefix resumes relocate only worktrees in
  `git worktree list` (also `prunable`) or named `<slug>-<hashPath(primary)>[-n]` (`session-paths.ts:238-300`);
  OmpDeck's `<branch>-<repo>` names are not recognized after `git worktree remove` (rc 1 "belongs to a directory
  that no longer exists").
- omp's picker in the primary lists the primary bucket plus `worktreeSessionDirs` siblings (same rules).
