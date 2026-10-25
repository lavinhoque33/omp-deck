# Sharpshooter project memory facts

Facts behind Wave 17 (project sync "Memory" part). Verified 2026-10-24 against omp 18.12.6 sources in
`~/PersProject/oh-my-pi` (`packages/coding-agent/src/sharpshooter/`, `src/mnemopi/config.ts`,
`packages/utils/src/{dirs,file-lock}.ts`, `crates/pi-natives/src/file_lock/`) and the real store on this machine.

## Layout (`sharpshooter/paths.ts:1-47`)

- Root: `getMemoriesDir(agentDir)/sharpshooter` = `<agentDir>/memories/sharpshooter`. `getMemoriesDir` is
  `agentSubdir(agentDir, "memories", "state")` (`dirs.ts:955`): on Linux/macOS, default profile, agent dir equal to
  the default `<configRoot>/agent` (no `PI_CODING_AGENT_DIR` override), and `$XDG_STATE_HOME/omp` **exists** →
  `$XDG_STATE_HOME/omp/memories` instead (`dirs.ts:365-399`; XDG flattens the `agent/` prefix).
- Bank (one per project, home-scoped, never in the working tree): `<root>/<bank id>/`
  - `architecture.md`, `product.md`, `style.md` — the memory (`SHARPSHOOTER_MEMORY_FILES`, injection order;
    ≤ 120 lines each after consolidation; empty file = nothing for that category; content rules forbid paths).
  - `queue/<sessionId>/<ts36>-<nonce>.json` — extracted deltas awaiting consolidation (session-bound; each must
    carry `evidence` = exact substring of the triggering user prompt, so they cannot be synthesized from memory files).
  - `state.json` — `{v:1,lastConsolidatedAt,lastResult?,lastError?}`; missing/corrupt → epoch state (harmless).
  - `.<file>.<pid>.<uuid>.tmp` — consolidation's temp files (write all, then rename each over its target).
- Readers (`backend.ts:52-71`) read only the three files; missing = empty.

## Bank id (`mnemopi/config.ts:206-292`, `sharpshooter/paths.ts:24`)

```
bankId(cwd)      = projectBankSegment(path.resolve(cwd || "."))
segment(root)    = limit(`${sanitize(basename(root)) ?? "default"}-${Bun.hash(root).toString(36)}`)
sanitize(v)      = v.trim() → replace /[^a-zA-Z0-9_-]+/g with "-" → strip leading/trailing "-"
                   → "" ⇒ undefined; else limit(result)
limit(name)      = name.length ≤ 64 ? name
                 : `${(name.slice(0, max(1, 63 - h.length)).replace(/-+$/,"") || "bank")}-${h}`
                   where h = Bun.hash(name).toString(36)
```

- `Bun.hash(string)` = Wyhash (Zig `std.hash.Wyhash`), seed 0, over the string's **UTF-8 bytes**, returning a
  `bigint` (u64); `.toString(36)` = lowercase base-36, no padding. Checked with bun 1.4.2: `Bun.hash(s)` ==
  `Bun.hash(TextEncoder.encode(s))` for `é`, `日本語`, `a😀b` (latin1 bytes differ).
- `.length`/`.slice` count UTF-16 units, but after `sanitize` the name is ASCII, so bytes = units.
- Real banks reproduced exactly with system bun: `/home/axeleratorx/PersProject/OmpDeck` → `OmpDeck-2k8vi2qa3ocpc`,
  `/home/axeleratorx/PersProject` → `PersProject-3tqqz73v1bxg7`, `…/JobApply` → `JobApply-33bu1quwj2ro8`,
  `…/Samsung Playground` → `Samsung-Playground-1zd7iwe9bjpld`, `/home/axeleratorx/projects/verity-frontend` →
  `verity-frontend-2o3h5695jzznj`.
- `path.resolve`: absolute, `.`/`..` resolved lexically, no trailing separator (except the root). On Windows it
  yields `C:\…` with backslashes and keeps the drive-letter case it was given.
- The id scheme changed once (#2412: no more git-root lookup). A backup must check that the computed bank exists
  and report "no memory" rather than guess.

## Consolidation lock (`file-lock.ts`, `pi-natives/src/file_lock/`)

- Consolidation runs under `withFileLock(<bank>/consolidate)` → lock path string `path.resolve(<bank>/consolidate) +
  ".lock"`; 50 attempts × 100 ms, then gives up for this round (`reason: "locked"`), no harm.
- Name: `omp-file-lock-{xxh64(path_utf8, 0x4f4d_502d_4c4f_434b):016x}{xxh64(path_utf8, 0x5049_2d46_494c_454c):016x}`.
  - Linux: bind a `UnixDatagram` to that **abstract** name; `AddrInUse` = held. Released on drop/exit.
  - Windows: `CreateMutexW(null, FALSE, "Global\\<name>")`; `ERROR_ALREADY_EXISTS` = held (close the handle).
  - Other Unix: `flock(LOCK_EX|LOCK_NB)` on the lock path file itself (created 0600).
- Consolidation reads the files, calls the model (seconds), then renames its outputs over the files: writing
  without the lock can be silently overwritten by output computed from the old content.
