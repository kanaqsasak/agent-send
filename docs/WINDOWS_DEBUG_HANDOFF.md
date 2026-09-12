# Windows Debugging Handoff

## Objective

Continue debugging the packaged Windows desktop app. The UI currently reports:

> Can't reach the local agent-send service.

The daemon is intended to be bundled inside the Tauri installer. Windows users must **not** install a separate daemon.

## Repository

```text
https://github.com/kanaqsasak/agent-send
```

Clone and work from the latest `main` branch. The latest relevant commit is `849d707` (the packaged-sidecar resource fix). The working release tag is `v0.1.0`.

## Confirmed user evidence

PowerShell showed the desktop process running, but no daemon process and no listener:

```powershell
Get-Process *agent-send* -ErrorAction SilentlyContinue
Test-NetConnection 127.0.0.1 -Port 8765
```

The startup diagnostic file contains:

```text
starting daemon
resource_dir=\\?\C:\Program Files\agent-send
daemon_sidecar=not_found
starting daemon
resource_dir=\\?\C:\Program Files\agent-send
daemon_sidecar=not_found
```

This proves the current failure occurs before daemon launch: the desktop shell cannot find a bundled daemon executable under the Tauri resource directory.

## Diagnostic locations

```powershell
Get-Content "$env:TEMP\agent-send-desktop-startup.log"
```

The daemon log, if startup gets far enough to create it, can be located with:

```powershell
Get-ChildItem "$env:APPDATA","$env:LOCALAPPDATA" `
  -Recurse -Filter "daemon.log" -ErrorAction SilentlyContinue |
  Select-Object FullName
```

Inspect the installed resource tree:

```powershell
Get-ChildItem "C:\Program Files\agent-send" -Recurse -Force |
  Select-Object FullName,Length
```

Look specifically for a file named similarly to:

```text
agent-send-daemon-x86_64-pc-windows-msvc.exe
```

## Relevant implementation

Desktop launcher:

```text
apps/desktop/src-tauri/src/lib.rs
```

Packaging configuration:

```text
apps/desktop/src-tauri/tauri.conf.json
```

CI packaging workflow:

```text
.github/workflows/package-desktop.yml
```

The launcher currently:

1. Gets `app.path().resource_dir()`.
2. Recursively searches for a file beginning with `agent-send-daemon-`.
3. Starts it with `--bind 127.0.0.1:8765`.
4. Passes an app-data identity path.
5. Writes startup diagnostics to `%TEMP%\agent-send-desktop-startup.log`.

The Tauri configuration declares both `externalBin` and explicit wildcard `resources` mappings for the daemon, CLI, and MCP binaries. The resource mappings place the staged target-specific files at the resource root so the packaged shell can locate them consistently on Windows.

## Packaging facts

The GitHub Actions packaging run succeeds for macOS, Windows, and Linux:

```text
https://github.com/kanaqsasak/agent-send/actions
```

The workflow stages Windows sidecars using:

```powershell
Copy-Item ../../target/release/agent-send-daemon.exe `
  "src-tauri/binaries/agent-send-daemon-$target.exe"
```

where `$target` comes from `rustc -vV`.

The Windows release asset has been manually replaced on the Release page multiple times. The current Release page is:

```text
https://github.com/kanaqsasak/agent-send/releases/tag/v0.1.0
```

## Likely root cause

The packaged installer does not contain the staged daemon sidecar where the desktop shell expects it, or Tauri is renaming/placing it differently than expected. Do not assume a runtime port or identity-directory problem until the installed resource tree proves the daemon exists.

## Recommended Windows-agent investigation

1. Clone the repository and inspect the exact installer build output.
2. Download the Windows Actions artifact from the latest successful packaging run.
3. Inspect the installer contents or install it in a clean Windows VM/user account.
4. Confirm whether `agent-send-daemon-*.exe` exists anywhere under the installed directory.
5. Inspect the Tauri build output before bundling. Record the exact staged filename and target triple.
6. Compare the staged filename with Tauri's expected `externalBin` naming convention.
7. The current fix uses explicit Tauri resource mappings to place the staged target-specific files at the resource root; retain the recursive launcher fallback while verifying the installer. If `externalBin` remains unreliable, use Tauri's official sidecar execution API (`tauri-plugin-shell`) or launch the exact mapped resource path.
8. Keep the launcher diagnostic log, and add a Windows integration/static packaging check that fails if the daemon resource is absent.
9. Build a fresh Windows installer and verify:

```powershell
Get-Process *agent-send* -ErrorAction SilentlyContinue
Test-NetConnection 127.0.0.1 -Port 8765
Get-Content "$env:TEMP\agent-send-desktop-startup.log"
```

10. Only after the daemon is found and running should identity storage, firewall, or API behavior be investigated.

## Important constraints

- The local automation API must remain loopback-only.
- The daemon must remain bundled; do not require a second installer.
- Preserve encrypted peer transport and folder capability restrictions.
- Do not commit generated build directories or staged binaries.
- Do not claim Windows support is fixed until a clean Windows install demonstrates a running daemon and successful `/v1/health` response.
- Update the Release page asset only after the new installer has been tested or its packaging contents have been verified.
