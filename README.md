# agent-send

Fast, private file transfer for people and AI agents.

`agent-send` is a lightweight desktop service that runs in the background, discovers trusted peers on the local network, and transfers files directly between explicitly approved folders. It is designed to feel as simple as LocalSend while providing a safe, scriptable interface for agents.

## Project status

Early architecture / MVP planning. The first target is a desktop daemon with a tray UI for macOS, Windows, and Linux.

## Product principles

- **Local-first:** no account, cloud relay, or required internet connection.
- **Safe by default:** receiving and agent access are opt-in, scoped, and auditable.
- **Fast:** stream files directly; avoid unnecessary copies and heavyweight services.
- **Automation-friendly:** a stable local API and CLI are first-class clients, not hacks.
- **Boring installation:** one signed installer, background startup, tray controls, and uninstall cleanup.

## Proposed stack

- **Rust:** background daemon, discovery, transfer protocol, filesystem policy, and local API.
- **Tauri 2:** small cross-platform desktop/tray shell with native OS integration.
- **Svelte + TypeScript:** thin settings and transfer UI.
- **mDNS/Bonjour:** peer discovery, with manual address entry as a fallback.
- **HTTPS initially; QUIC evaluation:** start with a simple interoperable protocol, then benchmark QUIC for large transfers and lossy networks.

GPUI is interesting, but it is currently a weaker fit for this product: it has a smaller ecosystem and more platform/UI integration work for a tray-first app. Tauri keeps the shell small while allowing the performance-critical path to remain Rust.

## Development

Prerequisites: Rust stable, Node.js, and the platform dependencies required by Tauri 2.

```sh
cargo test --workspace
```

Build and run the deterministic local transfer demo:

```sh
cargo build -p agent-send-cli
cargo run -p agent-send-cli -- demo
```

Run the background daemon locally (its config is read from
`~/.agent-send/config.json`; the identity is persisted at
`~/.agent-send/identity.json`):

```sh
cargo run -p agent-send-daemon -- --bind 127.0.0.1:0
```

Use a separate identity for development, disable LAN discovery, and query its
local health API with:

```sh
cargo run -p agent-send-daemon -- --identity-path /tmp/agent-send-dev.json --hidden --bind 127.0.0.1:8123
cargo run -p agent-send-cli -- health --addr 127.0.0.1:8123
```

The local automation API accepts only loopback addresses; stop the daemon with Ctrl-C. The daemon also starts a separate paired-peer TCP listener on `0.0.0.0:8742` by default. It never exposes the automation API on that listener. The desktop app will be added after the core protocol and security boundaries are validated.

## LAN networking and pairing

- Discovery publishes and browses the DNS-SD service `_agent-send._tcp.local.`.
  It uses mDNS multicast UDP port **5353** (`224.0.0.251` / `ff02::fb`).
- Authenticated peer traffic uses TCP port **8742** by default. Allow inbound TCP
  8742 and multicast UDP 5353 on a trusted private LAN if a host firewall blocks
  them. Wi-Fi client isolation can still prevent discovery and direct sockets.
- Discovery metadata is an untrusted endpoint hint. A transfer requires both
  devices to show and confirm the pairing code, trust the peer ID, and retain a
  distinct 32-byte out-of-band pairing secret. Encrypted frames bind both peer
  IDs and a monotonic sequence before folder/path policy receives any plaintext.
  Create an invitation with `POST /v1/pairings` on loopback; it returns a
  six-digit code and a 32-byte hexadecimal `pairing_secret`. Convey both to the
  other device out of band, create its matching invitation by including its
  `code` and `pairing_secret` with that peer's advertisement, then confirm each
  local invitation with `POST /v1/pairings/confirm`. Codes expire after five
  minutes and are consumed on successful confirmation. Re-pairing a peer
  replaces its secret; revoke with `DELETE /v1/peers/{peer_id}`.
- If mDNS is unavailable or isolated, add a peer's explicit `HOST:8742` address
  through the daemon integration/manual-peer adapter, then perform the same
  visible pairing and trust checks. A manual address is not a trust grant.

Confirmed peer records and pairing secrets are persisted through an isolated
trusted-peer storage abstraction. The current default is an atomic local JSON
file (`trusted-peers.json`, adjacent to the identity; owner-only permissions on
Unix). It is **not** an OS credential store and secrets are not encrypted at
rest by the daemon; protect the per-user data directory accordingly. A platform
credential-store adapter can replace this storage boundary in a future release.
The runnable binary still has no cross-device pairing UI, so an integration
must safely convey the invitation's secret and code out of band; socket traffic
never falls back to unauthenticated transport.

## Planned clients

1. Desktop tray app (macOS, Windows, Linux)
2. CLI (`agent-send send`, `agent-send receive`, `agent-send peers`)
3. Local agent API / MCP adapter
4. Mobile clients, if the protocol proves useful beyond laptops
