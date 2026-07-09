# Fetches the MIT-licensed Microsoft.Windows.Console.ConPTY redistributable pair
# (conpty.dll + OpenConsole.exe) from nuget.org and vendors it beside the conpty_osc spike.
# A .nupkg is a zip; we extract the native binaries for the current architecture.
#
# Pinned version — matched pair, update only together (see ARCHITECTURE.md ADR-002 / DECISIONS D-002).

$ErrorActionPreference = 'Stop'
$version = '1.24.260512001'
$pkg     = 'microsoft.windows.console.conpty'

# Map process arch -> the two differing folder conventions inside the package:
#   conpty.dll      lives under runtimes/win-<arch>/native/
#   OpenConsole.exe lives under build/native/runtimes/<arch>/
$arch = switch ($env:PROCESSOR_ARCHITECTURE) {
    'AMD64' { 'x64' }
    'ARM64' { 'arm64' }
    'x86'   { 'x86' }
    default { throw "Unsupported arch: $($env:PROCESSOR_ARCHITECTURE)" }
}

$root    = Split-Path -Parent $MyInvocation.MyCommand.Path
$vendor  = Join-Path $root 'conpty_osc\vendor'
$work    = Join-Path $env:TEMP "gmux-conpty-$version"
$nupkg   = Join-Path $work "$pkg.$version.nupkg"
$zip     = Join-Path $work "$pkg.$version.zip"
$url     = "https://api.nuget.org/v3-flatcontainer/$pkg/$version/$pkg.$version.nupkg"

New-Item -ItemType Directory -Force -Path $work, $vendor | Out-Null

Write-Host "Downloading $pkg $version ($arch)..."
Invoke-WebRequest -Uri $url -OutFile $nupkg -UseBasicParsing
Copy-Item $nupkg $zip -Force
Remove-Item (Join-Path $work 'extract') -Recurse -Force -ErrorAction SilentlyContinue
Expand-Archive -Path $zip -DestinationPath (Join-Path $work 'extract') -Force

$extract = Join-Path $work 'extract'
$sources = @{
    'conpty.dll'      = Join-Path $extract "runtimes\win-$arch\native\conpty.dll"
    'OpenConsole.exe' = Join-Path $extract "build\native\runtimes\$arch\OpenConsole.exe"
}

foreach ($f in $sources.Keys) {
    $src = $sources[$f]
    if (-not (Test-Path $src)) { throw "missing $f at $src" }
    Copy-Item $src (Join-Path $vendor $f) -Force
    $fi = Get-Item (Join-Path $vendor $f)
    Write-Host ("  vendored {0,-16} {1,10:N0} bytes  v{2}" -f $f, $fi.Length, $fi.VersionInfo.FileVersion)
}

Write-Host "Done -> $vendor"
