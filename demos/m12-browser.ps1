# M12 demo: the flag-gated WebView2 browser pane.
# Builds gmux with the browser feature, starts it, and asks it to open a page.
# Expect: the gmux terminal window plus a separate "gmux browser" window on example.com.

$repo = Resolve-Path (Join-Path $PSScriptRoot '..')
Push-Location $repo
try {
    cargo build -p gmux --features gmux-gui/browser
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

    $gmux = Join-Path $repo 'target\debug\gmux.exe'
    Start-Process $gmux
    Start-Sleep 4

    & $gmux browse https://example.com
    Write-Host "Sent 'gmux browse https://example.com' — a 'gmux browser' window should appear."
    Write-Host "Navigate again with:  gmux browse <url>    (same window re-navigates)"
} finally {
    Pop-Location
}
