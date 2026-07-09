# Runs the ConPTY + OSC passthrough spike.
#
# IMPORTANT: launch via Start-Process so the spike gets a REAL console. ConPTY needs a console
# context to bind the child's stdio to the pseudoconsole; under a console-less/pipe-only launcher
# (e.g. a CI/agent harness) the child won't attach and the test reports NO-GO. This is a launcher
# artifact, not a ConPTY limitation — see docs/research/m0-spikes.md.

$ErrorActionPreference = 'Stop'
$dir = $PSScriptRoot
$exe = Join-Path $dir 'target\debug\conpty_osc.exe'
if (-not (Test-Path $exe)) { cargo build --manifest-path (Join-Path $dir 'Cargo.toml') }

Remove-Item (Join-Path $dir 'result.txt'), (Join-Path $dir 'emit_diag.txt') -ErrorAction SilentlyContinue
$p = Start-Process -FilePath $exe -WorkingDirectory $dir -Wait -PassThru
Write-Host "ExitCode = $($p.ExitCode)  (0 = GO)"
Write-Host "--- result.txt ---";    Get-Content (Join-Path $dir 'result.txt')    -ErrorAction SilentlyContinue
Write-Host "--- child console ---"; Get-Content (Join-Path $dir 'emit_diag.txt') -ErrorAction SilentlyContinue
exit $p.ExitCode
