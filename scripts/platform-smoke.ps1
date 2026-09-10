# Safe, host-only smoke test. Does not alter firewall or autostart state.
[CmdletBinding()]
param([int]$Port = 18765)
$ErrorActionPreference = 'Stop'
$Root = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$Temp = Join-Path ([IO.Path]::GetTempPath()) ("agent-send-smoke.{0}" -f $PID)
$Process = $null
function Say([string]$Message) { Write-Host "[platform-smoke] $Message" }
function Skip([string]$Message) { Say "SKIP: $Message" }
try {
  New-Item -ItemType Directory -Path $Temp | Out-Null
  if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) { throw 'cargo is required' }
  if (-not (Get-Command curl.exe -ErrorAction SilentlyContinue)) { throw 'curl.exe is required' }
  Push-Location $Root
  cargo build -q -p agent-send-daemon
  $daemon = Join-Path $Root 'target\debug\agent-send-daemon.exe'
  if (-not (Test-Path $daemon)) { $daemon = Join-Path $Root 'target\debug\agent-send-daemon' }
  $url = "http://127.0.0.1:$Port/v1/health"
  Say "starting daemon in hidden mode on http://127.0.0.1:$Port"
  $Process = Start-Process -FilePath $daemon -ArgumentList @('--hidden', '--bind', "127.0.0.1:$Port", '--identity-path', (Join-Path $Temp 'identity.json')) -RedirectStandardOutput (Join-Path $Temp 'stdout') -RedirectStandardError (Join-Path $Temp 'stderr') -PassThru
  $ready = $false
  1..10 | ForEach-Object {
    if (-not $ready) {
      try { $body = curl.exe --fail --silent --show-error --max-time 1 $url; if ($body -match '"status"\s*:\s*"ok"') { $ready = $true } } catch { Start-Sleep -Milliseconds 200 }
    }
  }
  if (-not $ready) { throw 'health endpoint did not become ready' }
  Say 'PASS: daemon startup and /v1/health'
  Say 'PASS: hidden launch requested (--hidden disables discovery)'

  $bad = Start-Process -FilePath $daemon -ArgumentList @('--bind', "0.0.0.0:$($Port + 1)", '--identity-path', (Join-Path $Temp 'non-loopback.json')) -RedirectStandardOutput (Join-Path $Temp 'bad-out') -RedirectStandardError (Join-Path $Temp 'bad-err') -PassThru -Wait
  if ($bad.ExitCode -eq 0) { throw 'daemon accepted a non-loopback API bind' }
  Say 'PASS: non-loopback local API bind rejected'

  $Process.Kill(); $Process.WaitForExit(); $Process = $null
  try { curl.exe --fail --silent --max-time 1 $url | Out-Null; throw 'health endpoint remained reachable after shutdown' } catch [System.Management.Automation.RuntimeException] { }
  Say 'PASS: clean shutdown and local API unavailable'

  $files = @('register-linux.sh','register-macos.sh','register-windows.ps1','unregister-linux.sh','unregister-macos.sh','unregister-windows.ps1')
  foreach ($file in $files) { if (-not (Test-Path (Join-Path $Root "apps/desktop/scripts/$file"))) { throw "missing packaging script $file" } }
  Say 'PASS: autostart registration/unregistration scripts present'
  Skip 'autostart runtime registration (would modify the current user login state)'
  Skip 'LAN firewall/mDNS behavior (requires a trusted LAN and target host OS)'
  Skip 'installer install/uninstall (provide a target-OS installer for a non-destructive manual run)'
  Say "completed on $([Environment]::OSVersion.Platform) without claiming other operating systems"
} finally {
  if ($Process -and -not $Process.HasExited) { $Process.Kill(); $Process.WaitForExit() }
  Pop-Location -ErrorAction SilentlyContinue
  Remove-Item -Recurse -Force $Temp -ErrorAction SilentlyContinue
}
