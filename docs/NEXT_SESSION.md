# Next Session Handoff

## Current state

The repository is a working local prototype. Rust is pinned through `mise.toml` to Rust 1.88.0 because current Tauri dependencies require Edition 2024-era tooling.

Implemented and locally validated:

- Rust core path/capability policy with traversal and symlink protection
- Runnable daemon with loopback API, LAN mDNS lifecycle, and encrypted peer TCP transport
- Persisted pairing, one-time codes, expiration, revoke, and secret rotation
- Streamed transfers with hashes, cancellation, idempotency, size/file-count limits, and malformed-frame tests
- CLI demo and health/peer commands
- Scoped local agent API and stdio MCP adapter
- Tauri tray app with bundled daemon sidecar, autostart configuration, frameless tray popover, blur-to-hide, About/Quit context menu, and radar branding
- Packaging workflow and platform smoke scripts

## Verified commands

From the repository root:

```sh
cargo test --workspace
npm --prefix apps/desktop install
npm --prefix apps/desktop run build
./scripts/platform-smoke.sh
cargo run -p agent-send-cli -- demo
```

Run the desktop development app:

```sh
cd apps/desktop
npm run tauri:dev
```

`tauri:dev` stages the three development sidecars automatically. On macOS the
app starts the daemon and hides the Dock icon. Left-click the tray icon to open
the popover; blur hides it. Right-click exposes About and Quit.

## Known limitations / next work

1. Test native behavior and installers on actual Windows and Linux hosts.
2. Configure real signing/notarization credentials and verify release artifacts.
3. Replace placeholder app branding if the radar mark is not accepted.
4. Add a first-class CLI command for sending files between independently running daemon processes; current `demo` and socket integration tests exercise the transfer path but the CLI does not yet expose arbitrary-process send/receive.
5. Review pairing UX and secret storage for production; trusted-peer secrets are currently protected by owner-only local JSON files, not an OS credential store.
6. Run LAN tests across two physical laptops, including firewall, Wi-Fi isolation, sleep/wake, and interface changes.
7. Reconcile stale milestone checkboxes in `PLAN.md` only after the corresponding exit criteria are verified.

Do not claim cross-platform runtime or installer support based only on the macOS
checks above.
