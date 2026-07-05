# Runs a crate's ConPTY integration tests under a REAL console.
#
# ConPTY binds a child's stdio to the pseudoconsole only when the host process's stdout is a
# console (not a pipe/file), which `cargo test` under a pipe-stdio launcher (CI/agent harness)
# does not provide. Those tests are #[ignore]'d for plain `cargo test`; this runner builds the
# integration test binary and launches it via Start-Process (own console) with --include-ignored.
#
# Usage:  scripts/console-tests.ps1 <crate> <test-file>
#   e.g.  scripts/console-tests.ps1 gmux-pty spawn
#         scripts/console-tests.ps1 gmux-mux pane

param(
    [Parameter(Mandatory = $true)][string]$Crate,
    [Parameter(Mandatory = $true)][string]$TestFile
)

$repo = Resolve-Path (Join-Path $PSScriptRoot '..')
Push-Location $repo
try {
    & cargo test -p $Crate --test $TestFile --no-run
    if ($LASTEXITCODE -ne 0) { Write-Host "cargo --no-run failed ($LASTEXITCODE)"; exit $LASTEXITCODE }
    $exe = Get-ChildItem "target\debug\deps\$TestFile-*.exe" | Sort-Object LastWriteTime | Select-Object -Last 1
    if (-not $exe) { Write-Host "test binary '$TestFile-*.exe' not found"; exit 1 }
    $p = Start-Process -FilePath $exe.FullName -ArgumentList '--include-ignored', '--test-threads=1' -Wait -PassThru
    Write-Host "$Crate/$TestFile console tests exit code: $($p.ExitCode)  (0 = all passed)"
    exit $p.ExitCode
} finally {
    Pop-Location
}
