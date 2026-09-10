# Architecture

## Runtime

A single per-user `agent-send` daemon owns networking, transfers, and filesystem access. The desktop shell is a client of the daemon and can be closed without stopping transfers. A tray process starts the daemon at login and exposes pause/quit controls.

```text
Tray UI (Tauri/Svelte)
          │ authenticated local IPC/API
          ▼
Daemon (Rust)
 ├─ Peer discovery (mDNS + manual peers)
 ├─ Trust and pairing
 ├─ Transfer scheduler and resumable streams
 ├─ Folder capabilities / path policy
 ├─ Local human API + CLI API
 └─ Agent API / MCP adapter
          │ LAN only, encrypted and authenticated
          ▼
       Trusted peers
```

## MVP scope

- Discover peers on the same LAN and show online/offline state.
- Pair peers with a visible short code and persist trust locally.
- Send files and directories while preserving relative paths.
- Receive into a configurable default folder.
- Define named folders (for example `Downloads`, `workspace`, and `shared`) with read/write capability flags.
- Run headlessly after login with a tray control surface.
- Expose a localhost API and CLI for agents and scripts.
- Require confirmation for human-initiated unknown transfers; allow per-peer and per-folder agent policies.

## Agent interface

The local API is the trust boundary for automation. It must not expose arbitrary filesystem paths by default.

Conceptual operations:

- `peers.list`
- `folders.list`
- `transfers.send { peer, sources, destination_folder, idempotency_key }`
- `transfers.accept { transfer_id }`
- `transfers.cancel { transfer_id }`
- `events.subscribe`

The MCP adapter should be a separate process or thin adapter over this API. It should advertise only folders and peers the local policy permits. Every operation gets an audit record containing actor, peer, paths/capabilities, bytes, result, and timestamp.

## Security boundaries

- LAN traffic is encrypted and peer-authenticated; discovery metadata is not treated as trust.
- Pairing establishes a per-peer 32-byte secret and human-visible alias. A
  loopback pairing invitation returns that secret plus a six-digit code; both
  are conveyed out of band, the code expires after five minutes, and a
  successful confirmation consumes it. A new confirmed pairing rotates the
  stored secret. Trust can be revoked independently per peer.
- Shared folders are capabilities, not path strings. A capability contains a stable ID, root path, direction, and policy.
- Resolve and validate paths beneath the configured root; reject traversal, symlink escapes, and unexpected overwrite.
- Agent tokens are local-only, scoped, and revocable. The current daemon uses
  isolated local-file storage adapters; it does not implement an OS credential
  store.
- No inbound internet listener. Bind the agent API to loopback or a local IPC socket.

## Protocol direction

Begin with a versioned HTTP/JSON control protocol and streamed file bodies, compatible with LocalSend-style LAN deployment. Keep transport behind a trait so QUIC can be benchmarked without changing policy or UI. Transfers should support size/hash metadata, cancellation, progress, and resume tokens. The M3 loopback seam models this as a versioned request plus bounded (64 KiB) file chunks; `TransferEngine` owns capability checks and filesystem writes, while `LoopbackTransport` is replaceable by HTTP/QUIC. The current TCP peer adapter carries encrypted, length-delimited frames for a manifest, directories, file starts/chunks/completions, and a receiver result. It validates the destination capability before accepting content, writes files to temporary siblings before atomic rename, and records idempotency only after the verified hash and byte count complete.

`MdnsDiscovery` publishes and browses the `_agent-send._tcp.local.` DNS-SD service through the cross-platform `mdns-sd` adapter. It exposes only an endpoint, peer ID, alias, and API version; received advertisements enter the registry as untrusted hints and cannot authorize a peer channel. `PeerDiscovery` keeps the adapter replaceable, and `MockPeerDiscovery` makes discovery/trust tests deterministic.

`SecurePeerChannel` is the versioned peer control/transfer seam. A human-confirmed pairing must supply a distinct out-of-band 32-byte secret for each trusted peer; HKDF derives direction-specific keys and every control message or bounded 64 KiB file chunk is authenticated and encrypted with ChaCha20-Poly1305. Frames bind protocol version, sender, recipient, and a strictly monotonic sequence as associated data, so replay, misrouting, downgrade, and tampering are rejected before transfer policy sees plaintext. `PeerTransport` carries only these encrypted frames, while `TransferEngine` continues to own capabilities, hashes, cancellation, and idempotency.

The daemon owns a separate paired-peer TCP listener (default port 8742), starts/stops mDNS with its lifecycle, publishes its `_agent-send._tcp.local.` advertisement, and feeds received hints into the untrusted peer registry. The local automation API remains loopback-only. A listener accepts a transfer only when its peer ID and pairing secret are loaded from confirmed persisted trust; it never accepts secrets from DNS-SD or the socket. `PeerTrustStore` isolates this state from pairing and transport. The shipped `FilePeerTrustStore` writes an atomic JSON file with owner-only Unix permissions, but does not encrypt secrets at rest or use an OS credential store.

mDNS needs multicast UDP 5353 and the service needs inbound TCP 8742 on trusted LANs; Wi-Fi client isolation and platform firewall behavior remain deployment tests rather than claimed cross-platform guarantees. Explicit `HOST:8742` manual advertisements use the same untrusted-registry and pairing path when mDNS is unavailable. The runnable daemon has a persisted local request/confirm/revoke pairing flow, but still lacks a cross-device pairing UI and OS firewall/sleep/interface integration. An integration must convey invitation material out of band; the tested socket transport uses only persisted trust and never claims an OS credential-store implementation.

## Decisions to validate early

1. Whether mDNS works reliably across the target OS firewalls and Wi-Fi isolation modes.
2. QUIC versus HTTPS throughput, battery, and implementation complexity.
3. Best local IPC mechanism across macOS, Windows, and Linux.
4. Whether MCP should ship in the installer or remain an optional package.
