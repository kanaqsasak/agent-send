# agent-send Implementation Plan

> Working plan for AI agents contributing to `agent-send`. Read this document and
> `docs/ARCHITECTURE.md` before changing code. Keep changes small, tested, and
> scoped to the milestone being implemented.

## Vision

A fast, private, installable desktop service for sending files between trusted
laptops and to explicitly named folders. It should work like LocalSend for
humans, while exposing a safe local API/CLI/MCP surface for AI agents.

## Product constraints

- Target macOS, Windows, and Linux laptops.
- Runs as a background per-user service and can start at login.
- Tray UI can be closed while the daemon continues transfers.
- No required cloud account, relay, or internet connection.
- LAN traffic is encrypted and peer-authenticated.
- Agents cannot access arbitrary paths; they use scoped folder capabilities.
- Transfers must stream, report progress, support cancellation, and be safe to retry.
- Prefer small binaries, low idle CPU/memory, and no unnecessary copies.

## Milestones

### M0 — Foundation (current)

- [x] Repository, Rust workspace, core shared types, and baseline tests.
- [x] Initial architecture and security boundary documentation.
- [x] Add contribution instructions and CI.

### M1 — Daemon skeleton

- Add a Rust daemon crate with structured configuration and graceful shutdown.
  Local development run commands are documented in `README.md`; use
  `cargo run -p agent-send-daemon -- --bind 127.0.0.1:0` (or add
  `--hidden --identity-path /tmp/agent-send-dev.json --bind 127.0.0.1:8123`).
- Add a loopback-only local API with versioned request/response types.
- Add logging, health status, and a persistent identity/key placeholder.
- Add platform-neutral folder capability validation; reject traversal and symlink escapes.

**Exit:** daemon starts/stops deterministically, health endpoint works locally, and
folder policy tests cover allowed, traversal, and escape cases.

### M2 — Peer discovery and trust

- Implement mDNS discovery and manual peer entry.
- Define peer identity and pairing flow with a visible short code.
- Persist trusted peers and support revoke.
- Document firewall and network-isolation behavior.

**Exit:** two local daemon instances can discover, pair, list, and revoke each other.

### M3 — Transfers

- Implement versioned control handshake and encrypted streamed file transfer.
- Preserve directory-relative paths and reject unsafe archive/path names.
- Add hashes, progress, cancellation, overwrite policy, and resumability/idempotency.
- Add integration tests using two local daemon instances.

**Exit:** files and directories transfer reliably between paired peers, including
large files, cancellation, retry, and destination capability enforcement.

### M4 — Desktop shell and installation

- Add Tauri 2 desktop shell with Svelte/TypeScript UI.
- Add tray menu, notifications, settings, transfer status, and peer/folder views.
- Add login startup/autostart, hidden launch, installer packaging, and clean uninstall.
- Test macOS, Windows, and Linux packaging and firewall prompts.

**Exit:** a fresh user can install, pair, send/receive, configure folders, and run
headlessly after login.

### M5 — Agent interfaces

- Stabilize local API and ship `agent-send` CLI.
- Add optional MCP adapter exposing peers, folders, send, status, cancel, and events.
- Add scoped local agent tokens, approval policy, audit log, and idempotency keys.
- Include examples for common agent workflows.

**Exit:** an agent can send a file to a named peer/folder without receiving raw
filesystem authority or requiring UI automation.

### M6 — Hardening and performance

- Benchmark throughput, memory, idle CPU, startup, and discovery latency.
- Evaluate QUIC versus HTTPS without weakening policy or interoperability.
- Add fuzz/property tests for path handling and protocol parsing.
- Threat model, signed releases, update strategy, accessibility, and localization.

## Parallel implementation packets

The first fanout should implement independent foundations from the M0 baseline:

1. **Daemon skeleton:** own `crates/agent-send-daemon/**`; do not modify core types
   unless adding a focused, documented type is unavoidable.
2. **Folder security:** own `crates/agent-send-core/src/path_policy.rs` and its
   tests; do not implement network behavior.
3. **CI and contributor docs:** own `.github/**` and `CONTRIBUTING.md`; do not alter
   runtime code.
4. **Protocol research:** read-only investigation; return a recommendation for
   mDNS, HTTPS/QUIC, pairing, and LocalSend interoperability. Do not edit files.

After fanout, integrate and run the full workspace test suite in the main checkout.
Dependent work (discovery, transfers, Tauri shell, and agent API) must follow the
milestone order rather than being implemented as overlapping parallel edits.

## Engineering rules for agents

- Read `README.md`, `docs/ARCHITECTURE.md`, and this plan first.
- Keep transport, policy, UI, and agent API boundaries separate.
- Never broaden a folder capability into arbitrary filesystem access.
- Add tests with each behavior change; prefer deterministic local integration tests.
- Do not claim cross-platform support without testing the relevant platform or
  clearly marking it unverified.
- Update this plan checkboxes only when an exit criterion is actually met.
