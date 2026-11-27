# Bumps the unified semantic version across Cargo.toml and browser extension manifests.
# Usage: .\scripts\bump-version.ps1 -Version 0.3.0
param(
    [Parameter(Mandatory = $true)]
    [ValidatePattern('^\d+\.\d+\.\d+(-[0-9A-Za-z.-]+)?$')]
    [string]$Version
)

$ErrorActionPreference = "Stop"
$root = (Resolve-Path "$PSScriptRoot\..").Path

# 1. Update [workspace.package] version in Cargo.toml
$cargoPath = Join-Path $root "Cargo.toml"
$cargo = Get-Content $cargoPath -Raw
$updated = [regex]::Replace($cargo, '(?m)^(version\s*=\s*")[^"]+(")', "`${1}$Version`${2}", 1)
[System.IO.File]::WriteAllText($cargoPath, $updated, [System.Text.UTF8Encoding]::new($false))

# 2. Update browser extension manifests (numeric semver X.Y.Z required by Chrome/Firefox)
$extVersion = ($Version -split '-')[0]
foreach ($rel in "extension\manifest.json", "extension\manifest.firefox.json") {
    $path = Join-Path $root $rel
    if (Test-Path $path) {
        $text = Get-Content $path -Raw
        $text = [regex]::Replace($text, '("version"\s*:\s*")[^"]+(")', "`${1}$extVersion`${2}", 1)
        [System.IO.File]::WriteAllText($path, $text, [System.Text.UTF8Encoding]::new($false))
    }
}

Write-Output "Bumped Open Downloader to v$Version (extension manifest: v$extVersion)."
