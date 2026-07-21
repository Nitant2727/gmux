# Build a portable release zip: dist\gmux-<version>-x64.zip containing gmux.exe + docs.
# No installer tooling required (unsigned portable distribution until code signing lands).
#
# Usage:  scripts/package.ps1

$ErrorActionPreference = 'Stop'
$repo = Resolve-Path (Join-Path $PSScriptRoot '..')
Push-Location $repo
try {
    # cargo writes progress to stderr, which PS 5.1 would promote to a terminating error under
    # EAP Stop — relax around the build and gate on the exit code instead.
    $ErrorActionPreference = 'Continue'
    & cargo build --release -p gmux --features browser 2>&1 | ForEach-Object { "$_" } | Write-Host
    $ErrorActionPreference = 'Stop'
    if ($LASTEXITCODE -ne 0) { Write-Host "build failed ($LASTEXITCODE)"; exit $LASTEXITCODE }

    $version = (Select-String -Path "crates\gmux\Cargo.toml" -Pattern '^version\s*=\s*"([^"]+)"' |
        Select-Object -First 1).Matches[0].Groups[1].Value
    if (-not $version) { $version = "0.0.0" }

    $dist = Join-Path $repo "dist"
    $stage = Join-Path $dist "gmux-$version-x64"
    if (Test-Path $stage) { Remove-Item $stage -Recurse -Force }
    New-Item -ItemType Directory -Force $stage | Out-Null

    Copy-Item "target\release\gmux.exe" $stage
    Copy-Item "README.md", "LICENSE" $stage

    $zip = Join-Path $dist "gmux-$version-x64.zip"
    if (Test-Path $zip) { Remove-Item $zip -Force }
    Compress-Archive -Path "$stage\*" -DestinationPath $zip

    $size = [Math]::Round((Get-Item $zip).Length / 1MB, 1)
    Write-Host "packaged: $zip (${size} MB)"
} finally {
    Pop-Location
}
