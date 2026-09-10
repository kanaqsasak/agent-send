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

The Tauri bundle targets are configured for all three platforms:

- macOS: `.app` and `.dmg` (`npm run tauri:build -- --bundles app,dmg`)
- Windows: NSIS `.exe` and MSI (`npm run tauri:build -- --bundles nsis,msi`)
- Linux: AppImage, `.deb`, and `.rpm` (`npm run tauri:build -- --bundles appimage,deb,rpm`)

### Native prerequisites

Build on the target OS (the packaging workflow uses native GitHub-hosted
runners). In addition to Node.js 20+ and Rust stable:

- macOS: Xcode Command Line Tools and a macOS 10.15+ SDK.
- Windows: Microsoft C++ Build Tools (Desktop development with C++), WebView2,
  and PowerShell.
- Linux: `webkit2gtk-4.1`, `libayatana-appindicator3`, GTK, `librsvg2`,
  `patchelf`, `build-essential`, `curl`, `wget`, `file`, and `libssl-dev`
  development packages (the exact package names vary by distro).

### Daemon and CLI/MCP payloads

`src-tauri/tauri.conf.json` declares the daemon, CLI, and MCP adapter as Tauri
`externalBin` sidecars. No binaries are checked in or fabricated. A release
build must first compile each Rust binary in release mode, then copy it to
`src-tauri/binaries/<name>-<rust-target>` (and add `.exe` on Windows); Tauri
includes the matching target in the installer. The daemon is the runtime
service; CLI and MCP are optional executable companions for integrations. The
current shell still treats the daemon as an external per-user service and does
not launch or supervise these sidecars. Consequently, shipping the files does
not yet provide service registration, upgrades, or a complete pairing UX.

The packaging workflow performs this staging and names uploaded artifacts with
the app version, platform, and runner architecture. It never commits staged
binaries. Local `tauri:build` requires the three staged files; `npm run build`
does not.

### Signing and notarization

The workflow accepts (but does not invent) these repository or environment
secrets: `APPLE_CERTIFICATE`, `APPLE_CERTIFICATE_PASSWORD`,
`APPLE_SIGNING_IDENTITY`, `APPLE_ID`, `APPLE_PASSWORD`, `APPLE_TEAM_ID`,
`WINDOWS_CERTIFICATE`, `WINDOWS_CERTIFICATE_PASSWORD`, and
`TAURI_SIGNING_PRIVATE_KEY`/`TAURI_SIGNING_PRIVATE_KEY_PASSWORD` for future
update artifacts. Configure them in GitHub Actions before publishing; builds
without them remain unsigned. Never put certificates, private keys, or Apple
credentials in this repository. Linux artifacts are not signed by this
workflow.

The checked-in PNGs under `src-tauri/icons` are functional placeholder branding
and must be replaced before a public release. Firewall prompts, service
registration, update feeds, and cross-platform runtime testing remain known
limitations. Windows WebView2 and Linux WebKitGTK/AppIndicator packages are
runtime prerequisites; unsigned macOS builds may require Gatekeeper approval.
