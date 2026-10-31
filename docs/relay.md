# Self-hosting the OmpDeck relay

`omp-deck-relay` connects your desktop's OmpDeck gateway with your paired phones when they are not on
the same network. Both sides open an outbound WebSocket to the relay; the relay forwards their
messages. It is self-hosted only: there is no public relay. Design background:
[`docs/notes/mobile-relay.md`](notes/mobile-relay.md).

## What the relay can and cannot see

Every message between a phone and the gateway is end-to-end encrypted (Noise) before it reaches the
relay, and the relay holds no key that decrypts it.

| The relay sees | The relay never sees |
|---|---|
| room ids and relay-assigned peer ids | prompts, transcripts, terminal output, files, commands |
| connection times, client IP addresses, message sizes and timing | device or gateway keys, the pairing PSK |
| room secrets and admission tokens (the invite's one-use pairing token included) in each connection's hello, held only in memory; SHA-256 hashes of them | — (secrets and tokens are never stored or logged) |

Logs contain room ids, peer ids, close codes and addresses; never secrets, tokens, token hashes or
payload bytes. The wire libraries' own logging (`tungstenite`, `tokio_tungstenite`, `rustls`,
`tokio_rustls`, `hyper`, `hyper_util`) is clamped to `info` whatever `RUST_LOG` says, because their
trace output prints raw frames. A relay operator who only routes can drop or delay traffic, or
record metadata, but cannot read or forge it.

**The relay host also serves the phone app** (slice 4: the PWA loads from the relay's own origin).
Whoever controls the relay *host* can therefore serve modified app code that reads the phone's keys
and acts with every paired phone's permissions; for a phone with Control that means running commands
on your desktop. End-to-end encryption protects against a relay that only routes, the network path
and log readers, not against the server delivering the app. Secure and patch the relay host like the
desktop itself, and restrict who can administer it.

## Install

The relay is a single binary (Linux or Windows, about 5 MB) plus the phone app's static files. A
relay release archive keeps them side by side, and the relay finds `web/` next to its binary on its
own:

```text
omp-deck-relay-<version>-x86_64-linux/
  omp-deck-relay        (omp-deck-relay.exe on Windows)
  web/                  the built phone app: index.html, sw.js, manifest.webmanifest, assets/…
  omp-deck-relay.service, relay.md, licences
```

Each release ships `omp-deck-relay-<version>-x86_64-linux.tar.gz` (with a `.sha256`) in this layout. Copy the binary
and `web/` to the same directory, or put `web/` at `/usr/share/omp-deck-relay/web`. On other platforms, build both
from a checkout of this repository (the app needs [bun](https://bun.sh) 1.4.2):

```sh
cargo build --release -p deck-relay
install -m 0755 target/release/omp-deck-relay /usr/local/bin/
(cd pwa && bun install --frozen-lockfile && bun run build)
sudo mkdir -p /usr/share/omp-deck-relay
sudo cp -r pwa/dist /usr/share/omp-deck-relay/web
```

### systemd (recommended on Linux)

[`packaging/relay/omp-deck-relay.service`](../packaging/relay/omp-deck-relay.service) runs the relay as
a dynamic, unprivileged user with a hardened sandbox, keeps state in `/var/lib/omp-deck-relay` and
listens on `127.0.0.1:8080` behind a TLS reverse proxy.

```sh
sudo install -m 0644 packaging/relay/omp-deck-relay.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now omp-deck-relay
journalctl -u omp-deck-relay -f
```

Enroll rooms as the service's own user so `rooms.json` stays readable by it (see
[Enrolling a room](#enrolling-a-room)). Change flags with `sudo systemctl edit omp-deck-relay`
(clear `ExecStart=` first, then set the new one).

### Container

[`packaging/relay/Containerfile`](../packaging/relay/Containerfile) is a multi-stage build (bun 1.4.2
stage that builds the phone app into `/usr/share/omp-deck-relay/web`, Rust builder, Debian slim
runtime, non-root user, state volume). Build from the repository root:

```sh
podman build -f packaging/relay/Containerfile -t omp-deck-relay .
podman run -d --name omp-deck-relay -p 127.0.0.1:8080:8080 \
  -v omp-deck-relay:/var/lib/omp-deck-relay omp-deck-relay \
  serve --listen 0.0.0.0:8080 --state-dir /var/lib/omp-deck-relay \
  --trust-forwarded-for --trusted-proxy 10.88.0.1
```

Inside the container the host's proxy connects from the bridge gateway (rootful podman `10.88.0.1`, docker
`172.17.0.1`; rootless podman with slirp4netns `10.0.2.2`; when unsure, use the source address the relay logs for the
proxy's connections), so name it with `--trusted-proxy`; `X-Forwarded-For` from any other peer is ignored.
Without `--trust-forwarded-for` behind a proxy every client shares the proxy's address, so the
per-address limit (32) becomes a global cap. `docker` works the same way.

## Serving the phone app

Phones open the companion app (a PWA) from the relay's own origin, `https://relay.example.com/`, and
its WebSocket connects back to the same origin. The relay serves it next to its WebSocket routes:

- **Where from.** `--web-dir <dir>`; without it the relay uses `web/` next to its binary, then
  `/usr/share/omp-deck-relay/web` (the container image). If none exists it serves no app and says so
  once in the log at startup; the WebSocket routes work either way. An explicit `--web-dir` that
  cannot be read stops startup.
- **Read once.** Every file is read into memory at startup (at most 64 MiB in total, precompressed
  siblings included); restart the relay after updating the app. Names starting with `.` are skipped.
  Symlinks are followed only within the directory: one that leads outside it stops startup.
- **Paths.** `/` serves `index.html`; every other path serves the file of that name or answers 404.
  There is no SPA fallback, `/v1/*` never serves files (the relay protocol's namespace), and
  `/v1/gateway`, `/v1/device` and `/healthz` always win. Only `GET` and `HEAD` are served; other
  methods on a file answer 405.
- **Compression.** A `name.br` or `name.gz` next to `name` is sent instead of it (with
  `Content-Encoding`) when the request's `Accept-Encoding` allows it, brotli first; every file
  response carries `Vary: Accept-Encoding`. The siblings themselves are not served as files.
- **Revalidation.** Each body has a strong `ETag` (a hash of its content); a matching
  `If-None-Match` gets `304 Not Modified`.
- **Caching.** `assets/*` (the build's content-hashed names) is
  `Cache-Control: public, max-age=31536000, immutable`; everything else (`index.html`, `sw.js`,
  `manifest.webmanifest`, icons) is `no-cache`, so a new app version reaches phones on their next
  load. Content types are explicit (`.webmanifest` is `application/manifest+json`, `.js`
  `text/javascript`; also `.html`, `.css`, `.svg`, `.png`, `.woff2`, `.ico`, `.json`, `.txt` and a
  few more); unknown extensions are `application/octet-stream`.
- **Security headers** on every response from the app's paths (404 and 405 included):

  | Header | Value |
  |---|---|
  | `Content-Security-Policy` | `default-src 'none'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data: blob:; font-src 'self'; connect-src 'self'; manifest-src 'self'; worker-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'` |
  | `Referrer-Policy` | `no-referrer` |
  | `X-Content-Type-Options` | `nosniff` |
  | `Cross-Origin-Opener-Policy` | `same-origin` |
  | `Cross-Origin-Resource-Policy` | `same-origin` |
  | `X-Frame-Options` | `DENY` |
  | `Permissions-Policy` | `camera=(), microphone=(), geolocation=(), payment=(), usb=()` |
  | `Strict-Transport-Security` | `max-age=31536000`, only with `--tls-cert` (behind a proxy, set it there if you want it) |

  `style-src 'unsafe-inline'` is there for the terminal view: xterm.js injects `<style>` elements.
  Scripts stay strictly same-origin. `connect-src 'self'` covers the relay's own `wss://` origin, so
  the app's WebSocket needs no extra entry; that also means the app only talks to the relay it was
  loaded from.

Reverse proxies need nothing extra: the [Caddy](../packaging/relay/Caddyfile.example) and
[nginx](../packaging/relay/nginx.conf.example) examples already pass every path through. Do not let
the proxy strip or rewrite the headers above.

## TLS

Phones only connect over `wss://`, so the relay needs a certificate. Two ways:

1. **Reverse proxy (recommended).** Run the relay on `127.0.0.1:8080` and let the proxy terminate TLS:
   - [Caddy](../packaging/relay/Caddyfile.example): automatic certificates, WebSockets work out of the
     box.
   - [nginx](../packaging/relay/nginx.conf.example): needs the `Upgrade`/`Connection` headers and a
     `proxy_read_timeout` above 75 s (nginx closes idle proxied connections after 60 s by default).

   Pass `--trust-forwarded-for` so the per-IP limits apply to the real client address (the last
   `X-Forwarded-For` hop, the one your proxy appends) instead of the proxy's. The header is honoured
   only from a loopback peer or one listed with `--trusted-proxy <ip|cidr>` (repeatable); any local
   user can connect from loopback and forge it. With a CDN or a second proxy in front, or a hop that
   carries a port, every client falls back to one shared bucket. `--trust-forwarded-for` together
   with `--tls-cert` is refused (clients then connect directly).
2. **In-process TLS.** `--tls-cert <pem> --tls-key <pem>` (rustls; certificate chain in PEM, key as
   PKCS#8, PKCS#1 or SEC1 PEM). Restart the relay after renewing the certificate. Binding port 443
   needs `CAP_NET_BIND_SERVICE` (see the comment in the systemd unit).

`GET /healthz` answers `200 ok` for load balancers and uptime checks.

OmpDeck's gateway connects to the relay itself (rustls) and trusts the public web PKI roots it ships
with (webpki-roots), not the operating system's certificate store: use a certificate from a public CA
such as Let's Encrypt. Certificates from a private CA and HTTP(S) proxies between OmpDeck and the
relay are not supported. Plain `ws://` relay URLs are accepted only for a loopback host
(`localhost`, `127.0.0.0/8`, `[::1]`), because without TLS the room secret and device tokens would
cross the network in clear text.

## Enrolling a room

A room is one gateway (one desktop) and its paired devices. The relay only admits gateways of
enrolled rooms, so it is never an open service.

```sh
# plain install
omp-deck-relay enroll --state-dir /var/lib/omp-deck-relay --label "work laptop"
# systemd unit: run as the service's dynamic user
sudo systemd-run --pipe --wait --quiet -p DynamicUser=yes -p User=omp-deck-relay \
  -p StateDirectory=omp-deck-relay \
  /usr/local/bin/omp-deck-relay enroll --state-dir /var/lib/omp-deck-relay --label "work laptop"
# container
podman exec omp-deck-relay omp-deck-relay enroll --state-dir /var/lib/omp-deck-relay --label "work laptop"
```

It prints the room id and the **room key** (`ompdeck-relay-v1:…`). The key is shown only once and is
not stored on the relay (only `sha256(secret)` is); treat it like a password. It belongs in OmpDeck
together with the relay URL (`wss://relay.example.com`): **Settings → General → Mobile access → Set relay…**
(Mobile access must be on). The card then shows the link state; **Pair a device** opens a QR code for the
phone. A lost key cannot be recovered: remove the room and enroll a new one.

The running relay picks up a new room at the first gateway hello for it (within about a second) and
removed rooms within 30 s.

```sh
omp-deck-relay list --state-dir /var/lib/omp-deck-relay            # id, created (UTC), label
omp-deck-relay remove --state-dir /var/lib/omp-deck-relay <room id>  # disconnects its gateway and devices
```

`enroll` and `remove` hold an exclusive lock on `<state-dir>/rooms.lock` while they update
`rooms.json`, so concurrent commands never lose each other's changes. Leave the (empty) lock file in
place.

## Limits and flags

`omp-deck-relay serve --listen <addr:port> --state-dir <dir> [flags]`; `omp-deck-relay --help` lists
everything. Defaults:

| Limit | Default | Flag | When exceeded |
|---|---|---|---|
| connections in total (WebSockets plus sockets still in the TLS handshake or HTTP request) | 1024 | `--max-connections` | socket closed right after accept |
| connections per client address (IPv4-mapped IPv6 counts as IPv4; IPv6 per /64) | 32 | `--max-connections-per-ip` | socket closed right after accept; through a trusted proxy (the socket peer is the proxy) HTTP 429 before the upgrade instead |
| concurrent connections per device token | 2 | — | close 4029 (a half-open connection keeps its slot until the 75 s idle timeout) |
| device admissions per token | 2/s, burst 4 | — | close 4029 before the gateway hears of it (checked before the room rate, so one token holder cannot use up the room's admissions) |
| device admissions per room | 10/s, burst 20 | `--device-admissions-per-sec` (burst 2×) | close 4029 before the gateway hears of it |
| devices per room | 16 | `--max-devices-per-room` | close 4029 |
| device inbound bytes | 1 MiB/s, burst 4 MiB | `--device-bytes-per-sec` (burst scales 4×) | close 4029 |
| device inbound messages | 500/s, burst 1000 | `--device-messages-per-sec` (burst 2×) | close 4029 |
| gateway inbound bytes | 8 MiB/s, burst 16 MiB | `--gateway-bytes-per-sec` (burst 2×) | close 4029 |
| gateway inbound messages | 4000/s, burst 8000 | `--gateway-messages-per-sec` (burst 2×) | close 4029 |
| outbound queue per connection | 256 messages or 4 MiB | `--queue-messages`, `--queue-bytes` | close 4008. Toward the gateway, device data may fill at most ¼ of the queue per device token (both its connections, counted across reconnects, released as the gateway reads) and ¾ in total: the sending device is closed 4008 and the gateway stays. A gateway that stops reading is still dropped once the rest fills (taking its devices offline) |
| message size | 65 535 bytes (device), 65 540 (gateway) | — (protocol) | close 4013 |
| silence | 75 s (pings every 25 s) | `--idle-timeout-secs` | close 1000 |
| HTTP request headers (counted from accept, or the end of the TLS handshake with `--tls-cert`; on a keep-alive connection from the next request's first byte) | 10 s | — | socket closed |
| idle HTTP keep-alive (no traffic either way between requests) | 30 s | — | socket closed |
| stalled HTTP response write (no progress) | 30 s | — | socket closed |
| HTTP requests per connection | 100 | — | the last response carries `Connection: close` |
| TLS handshake (`--tls-cert`) | 10 s | — | socket closed |
| first message (hello) | 10 s | — | close 4001 |

Other flags: `--trust-forwarded-for`, `--trusted-proxy`, `--device-admissions-per-sec`, `--tls-cert`/`--tls-key`,
`--web-dir` ([Serving the phone app](#serving-the-phone-app)).
Logging: `RUST_LOG` (default `info`;
`debug` adds per-connection detail). A client that vanishes without a WebSocket close (process exit,
network loss) is logged as `connection lost`, not as a protocol violation. Behind
`--trust-forwarded-for` the total cap also counts the proxy's idle keep-alive connections. The
inbound rate limits apply from the first message on: the hello and any ping before it are charged.

The OmpDeck gateway paces what it sends to stay inside the defaults: at most 6 MiB/s and 3000
messages/s over all phones, and at most 1 MiB (32 messages) unacknowledged per phone. Do not lower
`--gateway-bytes-per-sec`, `--gateway-messages-per-sec`, `--queue-messages` or `--queue-bytes` below
their defaults: the relay would close the gateway (taking every phone offline) or drop busy phones.

Close codes the relay uses: 1000 normal, 1001 relay shutting down, 1002 protocol violation (text or
undecodable message), 4001 bad or late hello, 4003 unauthorized (unknown room, wrong secret, unknown
or revoked device), 4004 the room's gateway is offline, 4008 slow consumer, 4009 replaced by a newer
gateway connection, 4013 message too large, 4029 rate limited. The gateway can also close a phone through
the relay with 4002 (the phone broke the application protocol), 4010 (the phone stopped
acknowledging what it received) or 4026 (the phone's app speaks a protocol version this desktop does
not support; update OmpDeck or the app). Close codes are unauthenticated hints: a phone never deletes
its pairing because of one. 4029 is always temporary (connection caps, admission rate, inbound rate
limits): phones retry with backoff up to about 75 s.

## Upgrading

Replace the binary and its `web/` directory together (or rebuild the image) and restart: the app is
read at startup only. SIGTERM/SIGINT (Ctrl+C on Windows) closes
every connection with code 1001 and exits within 5 s; gateways and devices reconnect on their own with
backoff. Admission tokens live only in memory: each gateway re-registers its set right after
reconnecting, so a restart needs no migration. `rooms.json` is the only persistent state.

The relay protocol (`/v1/gateway`, `/v1/device`) is frozen: an incompatible change ships as new
`/v2/*` paths served next to `/v1`, so a newer desktop or phone keeps working with an older relay and
the reverse.

## Backup

Back up `<state-dir>/rooms.json` (owner-only on Unix, inherits the state directory's ACL on Windows;
replaced atomically on every change; copying it while
the relay runs is safe). It holds room ids, hashes of the room secrets, labels and creation times; no
secret that lets anyone join a room. Restoring it brings back the same rooms, so existing desktops
keep working with the keys they already have. Without a backup, enroll new rooms and enter the new
keys in OmpDeck.
