# M5 demo: drive gmux entirely from the outside over the automation API.
# Launches the GUI, then — from this script, no gmux UI touched — lists panes, splits one,
# sends a command into it, captures the screen, and raises a notification.

$ErrorActionPreference = 'Continue'
$repo = Resolve-Path (Join-Path $PSScriptRoot '..')
$exe = Join-Path $repo 'target\debug\gmux.exe'
if (-not (Test-Path $exe)) { & cargo build -p gmux; if ($LASTEXITCODE -ne 0) { exit 1 } }

Write-Host '=== launching gmux ==='
$gui = Start-Process -FilePath $exe -PassThru
Start-Sleep -Seconds 3

Write-Host '=== hello ==='
& $exe hello

Write-Host '=== list-panes ==='
& $exe list-panes

Write-Host '=== split-pane -h ==='
$newPane = (& $exe split-pane -h) | Select-Object -Last 1
Write-Host "new pane: $newPane"
Start-Sleep -Seconds 2

Write-Host "=== send-keys into $newPane ==="
& $exe send-keys -t $newPane --enter "echo hello-from-the-api"
Start-Sleep -Seconds 2

Write-Host "=== capture-pane $newPane ==="
& $exe capture-pane -t $newPane

Write-Host '=== notify (toast if gmux unfocused) ==='
& $exe send-keys -t $newPane --enter "gmux notify --title 'M5 demo' --body 'API round-trip complete'"

Write-Host '=== list-panes (after) ==='
& $exe list-panes

Start-Sleep -Seconds 3
Stop-Process -Id $gui.Id -Force -ErrorAction SilentlyContinue
Write-Host 'demo done.'
