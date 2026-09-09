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

The desktop app will be added after the core protocol and security boundaries are validated.

## Planned clients

1. Desktop tray app (macOS, Windows, Linux)
2. CLI (`agent-send send`, `agent-send receive`, `agent-send peers`)
3. Local agent API / MCP adapter
4. Mobile clients, if the protocol proves useful beyond laptops
