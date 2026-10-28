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
| connection times, client IP addresses, message sizes and timing | device or gateway keys, pairing secrets |
| SHA-256 hashes of room secrets and device admission tokens | room secrets or admission tokens themselves (only in the hello, never stored or logged) |

Logs contain room ids, peer ids, close codes and addresses; never secrets, tokens, token hashes or
payload bytes. A malicious relay operator can drop or delay traffic, or record metadata, but cannot
read or forge it.

## Install

The relay is a single binary (Linux or Windows, about 5 MB). Release archives do not ship
it yet; build it from a checkout of this repository:

```sh
cargo build --release -p deck-relay
install -m 0755 target/release/omp-deck-relay /usr/local/bin/
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

[`packaging/relay/Containerfile`](../packaging/relay/Containerfile) is a multi-stage build (Rust
builder, Debian slim runtime, non-root user, state volume). Build from the repository root:

```sh
podman build -f packaging/relay/Containerfile -t omp-deck-relay .
podman run -d --name omp-deck-relay -p 127.0.0.1:8080:8080 \
  -v omp-deck-relay:/var/lib/omp-deck-relay omp-deck-relay \
  serve --listen 0.0.0.0:8080 --state-dir /var/lib/omp-deck-relay --trust-forwarded-for
```

Only add `--trust-forwarded-for` when the published port is reachable by your proxy alone (as with
`127.0.0.1:8080` above). `docker` works the same way.

## TLS

Phones only connect over `wss://`, so the relay needs a certificate. Two ways:

1. **Reverse proxy (recommended).** Run the relay on `127.0.0.1:8080` and let the proxy terminate TLS:
   - [Caddy](../packaging/relay/Caddyfile.example): automatic certificates, WebSockets work out of the
     box.
   - [nginx](../packaging/relay/nginx.conf.example): needs the `Upgrade`/`Connection` headers and a
     `proxy_read_timeout` above 75 s (nginx closes idle proxied connections after 60 s by default).

   Pass `--trust-forwarded-for` so the per-IP limits apply to the real client address (the last
   `X-Forwarded-For` hop, the one your proxy appends) instead of the proxy's.
2. **In-process TLS.** `--tls-cert <pem> --tls-key <pem>` (rustls; certificate chain in PEM, key as
   PKCS#8, PKCS#1 or SEC1 PEM). Restart the relay after renewing the certificate. Binding port 443
   needs `CAP_NET_BIND_SERVICE` (see the comment in the systemd unit).

`GET /healthz` answers `200 ok` for load balancers and uptime checks.

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
together with the relay URL (`wss://relay.example.com`): **Settings → Mobile access**. That settings
page is not wired up yet (a later implementation step); until then keep the key somewhere safe. A lost
key cannot be recovered: remove the room and enroll a new one.

The running relay picks up new rooms at the next gateway hello and removed rooms within 30 s.

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
| connections per client address | 32 | `--max-connections-per-ip` | socket closed right after accept; with `--trust-forwarded-for` (the socket peer is the proxy) HTTP 429 before the upgrade instead |
| devices per room | 16 | `--max-devices-per-room` | close 4029 |
| device inbound bytes | 1 MiB/s, burst 4 MiB | `--device-bytes-per-sec` (burst scales 4×) | close 4029 |
| device inbound messages | 500/s, burst 1000 | `--device-messages-per-sec` (burst 2×) | close 4029 |
| gateway inbound bytes | 8 MiB/s, burst 16 MiB | `--gateway-bytes-per-sec` (burst 2×) | close 4029 |
| gateway inbound messages | 4000/s, burst 8000 | `--gateway-messages-per-sec` (burst 2×) | close 4029 |
| outbound queue per connection | 256 messages or 4 MiB | `--queue-messages`, `--queue-bytes` | close 4008 (a slow gateway also takes its devices offline) |
| message size | 65 535 bytes (device), 65 540 (gateway) | — (protocol) | close 4013 |
| silence | 75 s (pings every 25 s) | `--idle-timeout-secs` | close 1000 |
| WebSocket upgrade (counted from accept, or from the end of the TLS handshake with `--tls-cert`; also ends idle HTTP keep-alive such as health checks) | 10 s | — | socket closed |
| TLS handshake (`--tls-cert`) | 10 s | — | socket closed |
| first message (hello) | 10 s | — | close 4001 |

Other flags: `--trust-forwarded-for`, `--tls-cert`/`--tls-key`. Logging: `RUST_LOG` (default `info`;
`debug` adds per-connection detail). A client that vanishes without a WebSocket close (process exit,
network loss) is logged as `connection lost`, not as a protocol violation. Behind
`--trust-forwarded-for` the total cap also counts the proxy's idle keep-alive connections. The
inbound rate limits apply from the first message on: the hello and any ping before it are charged.

Close codes the relay uses: 1000 normal, 1001 relay shutting down, 1002 protocol violation (text or
undecodable message), 4001 bad or late hello, 4003 unauthorized (unknown room, wrong secret, unknown
or revoked device), 4004 the room's gateway is offline, 4008 slow consumer, 4009 replaced by a newer
gateway connection, 4013 message too large, 4029 rate limited.

## Upgrading

Replace the binary (or rebuild the image) and restart. SIGTERM/SIGINT (Ctrl+C on Windows) closes
every connection with code 1001 and exits within 5 s; gateways and devices reconnect on their own with
backoff. Admission tokens live only in memory: each gateway re-registers its set right after
reconnecting, so a restart needs no migration. `rooms.json` is the only persistent state.

## Backup

Back up `<state-dir>/rooms.json` (owner-only, replaced atomically on every change; copying it while
the relay runs is safe). It holds room ids, hashes of the room secrets, labels and creation times; no
secret that lets anyone join a room. Restoring it brings back the same rooms, so existing desktops
keep working with the keys they already have. Without a backup, enroll new rooms and enter the new
keys in OmpDeck.
