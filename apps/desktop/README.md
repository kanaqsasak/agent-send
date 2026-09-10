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

## Checks and packaging

Run these commands from `apps/desktop`:

```sh
npm run build                 # TypeScript check + Vite frontend build
npm run tauri:build           # build host-platform installer(s)
npx tauri info                # inspect native prerequisites and target
```

`tauri:build` produces the host platform's configured formats:

- macOS: `.app` and `.dmg` (`npm run tauri:build -- --bundles app,dmg`)
- Windows: NSIS `.exe` and MSI (`npm run tauri:build -- --bundles nsis,msi`)
- Linux: AppImage, `.deb`, and `.rpm` (`npm run tauri:build -- --bundles appimage,deb,rpm`)

The shell starts hidden in the Tauri configuration. A normal launch shows the
window; the autostart plugin passes `--hidden`, leaving only the tray visible.
Closing the window hides it, while the tray Quit action exits the shell. The
tray and autostart integration are configured, but the daemon is intentionally
an external per-user service: the lifecycle seam does not start or stop a
process until the daemon's packaging contract is defined. `AGENT_SEND_DAEMON_URL`
can still point the shell at a separately managed daemon.

The checked-in PNGs under `src-tauri/icons` are functional placeholder branding.
Replace them before release and add platform signing assets. Signing,
notarization, update feeds, and a bundled daemon are not configured.

Packaging is native-build only: build macOS on macOS, Windows on Windows, and
Linux on Linux (or use a separately validated cross-build/CI image). Linux
WebKitGTK/AppIndicator packages and Windows WebView2 are runtime prerequisites;
macOS first launch may require the usual Gatekeeper approval for unsigned apps.
Firewall prompts, service registration, and signed release behavior remain
platform validation work.
