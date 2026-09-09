# agent-send desktop shell

This is the first Tauri 2 desktop shell. It deliberately stays a thin client:
the Rust daemon remains a separate per-user process and owns health, transfers,
and filesystem access. The shell polls `GET /v1/health` and exposes show/hide/quit
from its tray menu. Closing the window hides it without stopping the daemon.

## Development

Requirements: Node.js 20+, Rust stable, and Tauri 2 platform dependencies.

```sh
npm install
npm run tauri:dev
```

To use a daemon on another loopback port, set `AGENT_SEND_DAEMON_URL`, for example
`AGENT_SEND_DAEMON_URL=http://127.0.0.1:9000 npm run tauri:dev`. The default is
`http://127.0.0.1:8765`; the current daemon/CLI can be started separately and
queried with `agent-send health --addr HOST:PORT`.

Platform prerequisites are documented by Tauri:

- macOS: Xcode Command Line Tools (macOS 10.15+).
- Windows: Microsoft C++ Build Tools (Desktop development with C++) and WebView2.
- Linux: `webkit2gtk-4.1`, `build-essential`, `curl`, `wget`, `file`, `libssl-dev`,
  `libayatana-appindicator3-dev`, and GTK/related development packages for the
  distro (see <https://tauri.app/start/prerequisites/>).

## Checks and builds

```sh
npm run build                 # TypeScript check + Vite frontend build
npm run tauri:build           # frontend plus Tauri bundle for the host platform
```

`tauri:build` is configured to produce host-platform bundles, but signing,
notarization, CI matrices, and a bundled daemon are not configured yet. These
are not release installers. The `DaemonClient` in `src/main.ts` is the seam for
adding a managed sidecar or platform service once the daemon packaging contract
is settled.
