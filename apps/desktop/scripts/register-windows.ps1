param([Parameter(Mandatory=$true)][string]$AppPath)
$runKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run'
New-Item -Path $runKey -Force | Out-Null
Set-ItemProperty -Path $runKey -Name 'agent-send' -Value ('"{0}" --hidden' -f $AppPath)
Write-Host 'Registered agent-send for the current user'
