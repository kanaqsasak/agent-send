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
- Pairing establishes a device key and human-visible alias. Trust can be revoked independently per peer.
- Shared folders are capabilities, not path strings. A capability contains a stable ID, root path, direction, and policy.
- Resolve and validate paths beneath the configured root; reject traversal, symlink escapes, and unexpected overwrite.
- Agent tokens are local-only, scoped, revocable, and stored using the OS credential store where available.
- No inbound internet listener. Bind the agent API to loopback or a local IPC socket.

## Protocol direction

Begin with a versioned HTTP/JSON control protocol and streamed file bodies, compatible with LocalSend-style LAN deployment. Keep transport behind a trait so QUIC can be benchmarked without changing policy or UI. Transfers should support size/hash metadata, cancellation, progress, and resume tokens. The M3 loopback seam models this as a versioned request plus bounded (64 KiB) file chunks; `TransferEngine` owns capability checks and filesystem writes, while `LoopbackTransport` is replaceable by HTTP/QUIC. Idempotency keys are recorded by the receiving engine, and files are written to a temporary sibling before an atomic rename.

Peer advertisements currently enter a deterministic in-process registry; the local API and trust persistence do not depend on a network discovery implementation. The future mDNS/Bonjour adapter should implement that seam and submit advertisements only (discovery metadata is never trust). This milestone intentionally does not open mDNS sockets, which keeps local tests deterministic and leaves firewall/Wi-Fi-isolation behavior for the adapter integration.

## Decisions to validate early

1. Whether mDNS works reliably across the target OS firewalls and Wi-Fi isolation modes.
2. QUIC versus HTTPS throughput, battery, and implementation complexity.
3. Best local IPC mechanism across macOS, Windows, and Linux.
4. Whether MCP should ship in the installer or remain an optional package.
