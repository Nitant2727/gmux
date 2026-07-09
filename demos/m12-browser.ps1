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

    Write-Host ""
    Write-Host "eval_js (M12 stage 2a): the BrowserPane::eval_js crate API is real — it runs a script"
    Write-Host "in the WebView2 and returns the JSON result over a channel with a 10s timeout. It is"
    Write-Host "NOT exposed over the automation pipe: eval needs a synchronous reply and the WebView2"
    Write-Host "lives in the GUI, not the daemon, so a pipe 'browser-eval' would require a daemon<->GUI"
    Write-Host "RPC bridge (out of scope here). Exercise eval_js via the ignored crate test on a"
    Write-Host "desktop with a WebView2 runtime:  cargo test -p gmux-browser -- --ignored"
} finally {
    Pop-Location
}
