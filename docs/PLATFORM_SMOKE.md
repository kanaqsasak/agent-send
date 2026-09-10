# Platform smoke checks

`platform-smoke.sh` and `platform-smoke.ps1` exercise the parts that can be
checked safely on the current host. They build the daemon, start it with
`--hidden` on a loopback address, query `/v1/health`, reject a non-loopback API
bind, and verify that the endpoint disappears after shutdown. They also verify
that all per-user registration and cleanup scripts are present.

Run from the repository root:

```sh
./scripts/platform-smoke.sh
# optionally choose another unused local port
AGENT_SEND_SMOKE_PORT=18766 ./scripts/platform-smoke.sh
```

On Windows PowerShell:

```powershell
.\scripts\platform-smoke.ps1
.\scripts\platform-smoke.ps1 -Port 18766
```

The scripts are intentionally non-invasive. They use a temporary identity,
do not enable or remove autostart, do not change firewall rules, and never run
an installer. The shell script requires `cargo` and `curl`; the PowerShell
script requires `cargo` and `curl.exe`. Use an unused port, or set the port
explicitly, if another process already owns the default.

## What still requires the target platform

A successful run is evidence only for the host on which it ran. It does not
verify macOS, Windows, or Linux when those operating systems are unavailable.
The scripts print explicit `SKIP` lines for checks that need a target host:

- **Autostart:** the desktop registers one per-user path: macOS LaunchAgent,
  Windows `HKCU\\Software\\Microsoft\\Windows\\CurrentVersion\\Run`, or
  Linux XDG autostart. Registration scripts live under
  `apps/desktop/scripts/`. Inspect or run the matching script on the target
  machine, then verify the native registration; do not run it in CI because it
  changes login state.
- **Hidden startup:** packaged startup passes `--hidden` to the desktop shell;
  the shell starts the bundled daemon before showing the window. The smoke
  test checks the daemon's corresponding hidden flag, but cannot prove a
  native login session without that OS.
- **LAN/firewall:** peer traffic uses inbound TCP `8742`; mDNS discovery uses
  multicast UDP `5353` (`224.0.0.251` / `ff02::fb`). Only a trusted private LAN
  should permit these paths. The automation API must remain on loopback and
  must not be opened in a firewall rule. Wi-Fi client isolation can still
  block peers.
- **Install/uninstall:** Tauri produces `.app`/`.dmg`, NSIS/MSI, and
  AppImage/deb/rpm bundles on their respective hosts. Build and install a
  target-host artifact, verify the bundled daemon starts and health responds,
  then uninstall and verify the app and per-user registration are gone. User
  data under the platform home directory is intentionally retained. The smoke
  scripts only check packaging helper presence and never claim installer
  coverage.

Do not report an unavailable OS as tested. Record the OS, package format,
installer version, registration path, firewall prompt/result, health response,
and clean-shutdown result for each target-host run.
