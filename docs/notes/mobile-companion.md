# Mobile companion: design contract

Status: step 1's in-process shared service is implemented; gateway/OS integration,
relay/pairing and the PWA remain planned. Decisions agreed on 2026-10-25: relay
connectivity, background gateway by default with an app-open-only option, installable
PWA, full control. The implementation boundary and command routes are recorded below.

## Product boundary

The phone controls sessions and projects on the desktop machine. omp, tools, git,
worktrees, provider credentials and backups continue to execute there. The PWA is
not a separate agent runtime, a session-copy/sync mechanism or an OS remote desktop.
The desktop GUI must not be required for session/project actions in background mode.
Linux and Windows are both targets; Android and iOS are PWA targets.

Full control means parity for OmpDeck's session/project actions, not just watching
transcripts or sending prompts. Maintain a command coverage matrix against the
existing menus, palette, status bar and context menus during implementation:

| Area | Required mobile surface |
|---|---|
| Sessions | All-project inventory, search, status, new/resume, restart, stop/kill, rename, fork, copy/move, delete, bulk operations and running-process RAM |
| Conversation | Live transcript, tool output, composer with attachments, interrupt, steer/follow-up semantics, pending questions/approvals, last-response copy and export |
| Runtime | Model/effort selection, plan, retry, handoff, compact variants, context and usage |
| Agents and history | Fleet dashboard, subagent transcripts/control, session tree, fork-at and navigation |
| Projects and git | Desktop-folder selection, worktree create/move/finish/remove, changes/diffs, stage/unstage/discard and review comments |
| Backup and settings | Config backup, project sync including sessions/memory, restore/delete confirmations, omp settings, update checks and update jobs |
| Workspace and notifications | Session navigation, explicit desktop tab/focus/layout actions, inbox, per-device push preferences and gateway/device settings |

Desktop paths refer to the desktop filesystem: a phone file picker is not a desktop
folder picker. Phone navigation does not steal desktop focus unless explicitly
requested. Export/download and clipboard actions target the initiating device;
"open on desktop" actions are named as such. OS-local operations (window raising,
OAuth browser authorization or installer elevation) require explicit lifecycle and
interaction designs in the matrix, not silent omission or pretend success.

## Service ownership

Add an `omp-deck-gateway` per-user daemon and a versioned shared command/event
contract. The desktop GUI uses private local IPC; the PWA uses the encrypted relay
transport. These are two clients of the same application service, not independent
session managers.

The shared service owns session identity/lifecycle, command arbitration, long jobs,
authoritative shared settings and notification events. Keep native rendering,
window geometry and device-local view state in the respective client. Do not let
the daemon and GUI independently rewrite a complete shared `AppConfig` snapshot.

Commands target stable session/process identities and expected generations, never
"whatever tab is active". Preserve the desktop injector's idle checks, draft
confirmation, overlay checks, control-key spacing, input hold and paste encoding.
Desktop typing and phone commands must share the same per-session arbitration.
Revalidate a destructive action against its target at execution time.

A remote request needs an ID, acknowledgement and outcome. On reconnect, recover
the outcome of an accepted request rather than blindly replaying it. In particular,
never automatically repeat prompts, deletes, moves, restores or updates. Event
streams need snapshots/resynchronization when a client has missed history.

Do not attach a second writer to an existing session file. Session-store mutations
still go through omp. Existing Google Drive config/memory restore exceptions and
their safety-copy/locking requirements remain unchanged.

### Current execution and terminal boundaries

- `deck-service` owns composer recognition, injection, model-switch tracking,
  per-session command outcomes, settings transactions and notification classification.
  The native desktop is its first client; it still owns windows, rendering and
  native interaction effects. There is no gateway process or private IPC yet.
- `deck_core::ctl::CtlCommand` still defines `Kill`, `Revive`, subagent-only `Chat`,
  `ForkAt` and `Navigate`. No runtime slash command was substituted with a presumed
  equivalent ctl command. Verify parity for the complete runtime-command set before
  any such migration.
- One `SessionController` owns the terminal writer. Native widgets borrow it.
  Headless typing uses target-checked `write_input`, which refuses input during an
  injected sequence instead of interleaving, queueing or replaying keystrokes.
- `subscribe_view` offers latest-value **state/text** snapshots for tmux, host and
  direct terminals without another backend attachment. It is not a styled/raw VT
  mirror. Slow state viewers skip intermediate snapshots rather than accumulating
  a queue.
- Host `Observe` provides VT replay/live output and ordered geometry without
  displacing or resizing the controller. Its only control is detach; forbidden
  mutations disconnect immediately. Each observer has a byte budget; a slow
  observer cannot block the controller. Older daemons return `Unsupported` without
  an attach fallback.
- Raw VT mirroring of the service-owned tmux/direct terminal is not implemented.
  The later mobile terminal transport must provide the required terminal fidelity
  before claiming full terminal parity; text snapshots must never be presented as
  that transport. The native terminal's existing fidelity is unchanged.
- Settings edits use field-level baseline compare-and-set under a cross-process
  lock. Native presentation state is device-scoped; clients no longer save stale
  complete shared snapshots. One `NotificationCenter` produces canonical events;
  OS delivery and activation remain native effects.

A structured conversation UI remains primary. Terminal access for TUI-only or
extension interactions must preserve the same input ownership, sizing and safety
rules; the phone must not steal desktop focus just by navigating.

## Relay and pairing

Both the gateway and PWA initiate outbound connections; no desktop port forwarding
or public control listener. Deploy an OmpDeck relay over WSS and serve the PWA over
HTTPS. The relay forwards opaque encrypted messages; desktop session content and
provider secrets must not be available to it. It can still observe connection
metadata, traffic sizes and timing.

Do not assume permission, capacity, compatibility or uptime for omp's public
`my.omp.sh` service. Upstream collab is a useful protocol/UI reference, but a
single-session collab room is not a machine-wide OmpDeck control API. Ship a
self-hostable relay; hosting provider/domain and an optional managed deployment
remain deployment decisions, not prerequisites for writing the local service.

Pair only during a short-lived, one-use pairing window explicitly opened on the
desktop. Show a QR code, authenticate the exchange with a vetted protocol/library,
and require desktop approval of the new device. Do not put permanent control keys
in a reusable link or send pairing secrets in HTTP query strings or logs.

Authenticate each paired device independently. Revocation disconnects active
connections and rejects future commands; credentials and push subscriptions must
not be restored to an authorized state by replay. Application encryption needs
peer authentication and replay protection, not just TLS or encryption with a
shared room secret. Freeze the exact protocol only after security review.

Treat the PWA delivery origin as trusted software: it serves code that can access
the phone's session data/keys. E2E encryption protects against the routing relay,
not malicious client JavaScript. Do not persist full transcripts or secrets in the
offline cache by default. Never expose desktop provider tokens or the local ctl
capability key as phone API credentials.

## Gateway lifetime and settings

Mobile access is opt-in. On enabling it, default to **Background** mode:

| Mode | Behavior |
|---|---|
| Off | No remote connections or push delivery; existing omp sessions are not killed |
| While OmpDeck is open | Run the same gateway while at least one GUI instance is connected; stop after clients/jobs have drained |
| Background (default when enabled) | Start at user login, survive closing the GUI, reconnect after network loss and restart after a daemon crash |

Background means independent of the GUI, not independent of power or the logged-in
user. Linux packaging should integrate a user service; Windows packaging should
integrate a per-user startup mechanism. Before-login/after-logout operation is a
separate OS capability and must not be promised by the default setting. Preserve
the user's omp environment, paths and profiles when starting outside a shell.

Guarantee one gateway per user/namespace. Mode changes and daemon updates must not
kill unrelated sessions, interrupt accepted destructive jobs or leave duplicate
autostart entries. App-open-only shutdown must account for multiple GUI windows.
Expose connected/offline status, relay URL, lifetime mode, paired-device list,
revocation and push preferences in desktop Settings -> Mobile access.

If the PC sleeps, shuts down or loses connectivity, show the last known state as
stale/offline and disable mutations. Do not silently queue commands for execution
when the machine later reconnects. A cached PWA shell is not an offline desktop.

## PWA and notifications

Use a responsive TypeScript/React PWA with manifest, service worker, install flow,
HTTPS, reconnect/resync and visible connection state. Reuse compatible upstream
collab transcript/tool rendering where practical, with an explicit dependency and
update strategy; do not treat upstream package internals as a stable public API.

A backgrounded PWA cannot be assumed to keep a WebSocket alive. Generate events on
the gateway, persist the inbox independently of live browser connections and send
Web Push through the browser vendor's push service. A push notification is a hint:
opening it loads authoritative state from the desktop. Use privacy-preserving
notification text by default. Browser permission, focus modes and OS delivery
policies mean push is not guaranteed immediate delivery.

On iOS/iPadOS, Web Push requires a Home Screen web app on 16.4+ and a permission
request triggered by user interaction. No Apple Developer Program membership is
required for Web Push. See [WebKit's platform documentation](https://webkit.org/blog/13878/web-push-for-web-apps-on-ios-and-ipados/).

Keep OmpDeck the only notification source: launched omp still receives
`PI_NOTIFICATIONS=off`. One canonical event stream feeds desktop/in-app/mobile
channels, with separate device delivery preferences and no duplicate turn-finished
events. Preserve focused-tab inbox behavior and desktop-unfocused popup policy.

## Implementation slices and gates

1. **Shared service and command contract.** Map every existing command, extract
   safe execution/state ownership, solve terminal single-writer/fan-out, preserve
   desktop behavior and define protocol capabilities/errors and config ownership.
2. **Gateway and OS integration** (implemented and released in 0.15.0; Linux and Windows runtime evidence in
   `contracts.md` "Mobile companion step 2: as built"). Headless lifecycle/jobs/watchers, private IPC,
   singleton startup, both lifetime modes, installer/uninstaller and update paths
   on Linux and Windows. Prove control with the GUI closed before networking.
3. **Encrypted connectivity.** Relay, pairing, device identity/revocation,
   acknowledgement/outcome recovery, reconnect/resync, resource bounds and a
   security review. No public deployment of an unauthenticated bridge.
4. **PWA full-control client.** Mobile navigation and every coverage-matrix action,
   terminal/approval flows, desktop-path browser, downloads/attachments, clear
   destructive confirmations, service worker and Web Push.
5. **End-to-end release proof.** Physical Android and iOS installed PWA runs across
   different networks, Linux and Windows gateways, GUI closed, simultaneous GUI
   use, sleep/network loss, reconnect during an action, revocation, and installation,
   mode changes, update and uninstall. A staged implementation does not reduce the
   agreed full-control scope.

## Step 1 implementation and command parity

The current contract is a Rust application-service API, not a serialized public
protocol. `SessionKey` includes mux kind, namespace and name; `SessionTarget` adds a
process-local generation. Restarted controllers and changed session files invalidate
stale actions and draft answers. The future IPC handshake must also bind requests to
a service-instance epoch; no cross-daemon-restart request persistence is claimed here.

| Existing action family | Execution route after the desktop cutover |
|---|---|
| Inventory, project/session search, running processes/RAM | Existing `deck_core::{sessions,inventory,mux,platform,fleet}` read/worker APIs |
| New, resume, attach, restart, stop/kill | `deck_service::backend`, `deck_core::mux::Mux`, `deck_service::host_remote::HostRemote`; tab/focus effects remain native |
| Fork/copy, move, delete and bulk operations | Existing `deck_core::omp::Omp`; shared `lifecycle::stop_then_move` / `stop_then_delete` preserve stop-first and live-writer retry rules |
| Handoff, compact variants, rename/auto-name, plan, retry, interrupt | `SessionController::submit(Command::Action)` using the one moved injector/composer implementation |
| Model and thinking effort | `Command::{SwitchModel,SetEffort}` plus `observe_model`; delivery and observed runtime effect remain separate |
| Terminal typing and paste | Native widget's existing encoder/hold checks; headless `write_input(target, bytes)` checks identity, ownership and hold before accepting already-encoded bytes |
| Tree fork-at/navigation and subagent kill/revive/chat | Existing `deck_core::ctl` operations; native link/confirmation presentation remains client-local |
| Changes, stage/unstage/discard/delete-untracked | Existing `deck_core::changes` executors and safety checks |
| Review comments | Client builds the review; `Command::Review` owns mode-aware insert/send, draft confirmation and input hold; only acknowledged delivery clears sent comments |
| Worktree create/inspect/remove, move and finish | Existing `deck_core::worktree`; `Command::{WorktreeMove,WorktreeFinish}` for injected operations |
| Usage, context, transcript and agent data | Existing `deck_core::{usage,status,agents,tree}` sources/control snapshots |
| Export and last-response copy | Shared `session_actions::export_html`; native clipboard/download presentation remains a client effect |
| Config backup, project sync/restore/delete and memory | Existing `deck_core::{backup,sharpshooter}` operations; confirmations and OAuth/native folder selection remain client interaction |
| OmpDeck preferences and omp configuration | `SettingsService` transactions for OmpDeck; existing `deck_core::omp` config operations for omp |
| omp/plugin install/update/channel and OmpDeck self-update | Existing core CLI/self-update executors and mux-backed command tabs; elevation/restart remain explicit native effects |
| Inbox and notifications | Shared `NotificationCenter`, keyed by full `SessionTarget`; native notifier/activation adapter only |
| Tabs, split layout, navigation, window/clipboard/folder effects | Native client state/effects; no remote desktop-focus or window-control endpoint is exposed |

Injected submissions carry `RequestKey { client, seq }` and an exact target.
Duplicates return the original request; a reused key for a different request is
refused, and evicted accepted sequences remain expired rather than executing again.
Pending requests and confirmations are never evicted. Errors distinguish stale,
exited, read-only, detached, busy, input-held, unavailable paste/model and invalid
request states; the gateway must preserve these distinctions instead of reporting
generic success.

`Queued`, `AwaitingConfirm` and `Writing` are not completion. `Delivered` means the
local PTY/remote transport accepted the bytes, **not** that omp or the host's child PTY
executed them. Model switches have a separate runtime verdict. At an explicit close,
the original injector boundary is preserved: an unacknowledged queued write becomes
`Unobserved` and may still arrive during detach. It is never counted as success,
does not clear review comments and is never automatically resent.

`SettingsService` re-reads and decodes the stored file once under the lock, merges
disjoint edits and refuses conflicting edits atomically. A global tab list, active
index and split form one CAS unit; folder layouts compare per folder key, including
additions/removals. Corrupt/unreadable files are not overwritten by defaults. Change
callbacks run outside the transaction worker. The desktop reloads external commits,
reconciles runtime effects, surfaces conflicts and retries conflict-rebased surviving
fields before its final shutdown flush (without retrying I/O failures).

Runtime verification used isolated session/config roots:

- Real omp headless rename through the service, deduplicated submission, two
  non-displacing state viewers and a read-only host VT observer.
- Separate-process settings merge/conflict; native two-process preference reload,
  external style adoption and backend adoption. A client whose peer selected tmux
  actually launched a new omp in the test tmux server; the old host process survived.
- Native palette rename with draft cancel/confirm, detach/reopen of the same omp
  process, selected-process stop, middle-click close and the focused tab's
  session-ended inbox entry.
- A real-PTY headless typing smoke: stale/held input was refused without queueing,
  then accepted input reached the child after the hold.

No provider prompt was submitted. Host tests additionally cover ordered joins and
resizes, forbidden input, slow peers, detach/exit and old-daemon refusal. Workspace
tests, formatting and Linux/Windows-target clippy passed. Native Linux runtime was
exercised; Windows runtime still requires Windows CI. Turn-finished classification
and desktop-popup policy have test coverage; an actual OS popup was not exercised
in this isolated native smoke. Gateway/PWA/mobile-device runtime remains unimplemented.

## Step 2 status

Implemented (0.15.0, follow-ups through 0.16.1): the per-user gateway, the desktop as its client, both lifetime
modes with systemd-user / Windows logon-task supervision, install/update/uninstall drain, and replacement of an
outdated owner. Step 1's "Windows runtime still requires Windows CI" is now answered for the transport, host,
real-omp live control and the GUI-through-gateway path (CI), plus the gateway CLI/e2e and task trigger measured
on Windows 11 (see `contracts.md`). Not shown by CI: `install.ps1` drain/resume, enabling Always on from the
Mobile access card on Windows, and gateway-originated toast delivery. Relay, pairing and the PWA (slices 3-4) remain
unimplemented.
