# Runs gmux-pty's ConPTY integration tests under a REAL console.
#
# ConPTY binds a child's stdio to the pseudoconsole only when the host process's stdout is a
# console (not a pipe/file). `cargo test` under a pipe-stdio launcher (CI/agent harness) fails
# that condition, so the output-checking tests are #[ignore]'d there. This runner launches the
# already-built test binary via Start-Process (which gives it its own console) and runs the full
# set with --include-ignored. Exit code 0 = all passed.

$repo = Resolve-Path (Join-Path $PSScriptRoot '..\..')
Push-Location $repo
try {
    & cargo test -p gmux-pty --test spawn --no-run
    if ($LASTEXITCODE -ne 0) { Write-Host "cargo --no-run failed ($LASTEXITCODE)"; exit $LASTEXITCODE }
    $exe = Get-ChildItem 'target\debug\deps\spawn-*.exe' | Sort-Object LastWriteTime | Select-Object -Last 1
    if (-not $exe) { Write-Host 'spawn test binary not found'; exit 1 }
    $p = Start-Process -FilePath $exe.FullName -ArgumentList '--include-ignored', '--test-threads=1' -Wait -PassThru
    Write-Host "gmux-pty ConPTY tests exit code: $($p.ExitCode)  (0 = all passed)"
    exit $p.ExitCode
} finally {
    Pop-Location
}
