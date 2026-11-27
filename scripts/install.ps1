# Installs Open Downloader for the current user on Windows:
# - Builds release binaries (`od`, `od-nmh`, and optionally `od-app`)
# - Copies them to %LOCALAPPDATA%\OpenDownloader\bin and records version.txt
# - Configures auto-startup (Task Scheduler silent daemon by default, or GUI Run key)
# - Optionally registers Chrome/Edge/Firefox native messaging host
# - Adds Start Menu shortcut (when GUI is installed) and adds bin dir to user PATH
#
# Usage:
#   .\scripts\install.ps1 [-ExtensionId <id>] [-Firefox] [-SkipBuild] [-SkipGui]
#                         [-AutoStartMode Daemon|App|None]
param(
    [string]$ExtensionId = "",
    [switch]$Firefox,
    [switch]$SkipBuild,
    [switch]$SkipGui,

    [ValidateSet("Daemon", "App", "None")]
    [string]$AutoStartMode = "Daemon",

    [string]$InstallDir = "$env:LOCALAPPDATA\OpenDownloader"
)

$ErrorActionPreference = "Stop"
$root = (Resolve-Path "$PSScriptRoot\..").Path
$binDir = Join-Path $InstallDir "bin"

# Read workspace version from Cargo.toml
$cargoToml = Get-Content (Join-Path $root "Cargo.toml") -Raw
$version = if ($cargoToml -match '(?m)^version\s*=\s*"([^"]+)"') { $Matches[1] } else { "0.0.0" }
Write-Output "Installing Open Downloader v$version ..."

if (-not $SkipBuild) {
    $pkgs = @("-p", "od-cli", "-p", "od-nmh")
    if (-not $SkipGui) { $pkgs += @("-p", "od-app") }
    & cargo build --release --manifest-path "$root\Cargo.toml" @pkgs
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }
}

New-Item -ItemType Directory -Force $binDir | Out-Null
$bins = @("od.exe", "od-nmh.exe")
if (-not $SkipGui) { $bins += "od-app.exe" }

foreach ($exe in $bins) {
    $src = Join-Path $root "target\release\$exe"
    if (-not (Test-Path $src)) {
        throw "$exe not found at $src — run 'cargo build --release' first"
    }
    Copy-Item $src $binDir -Force
}
Set-Content -Path (Join-Path $InstallDir "version.txt") -Value $version -NoNewline
Write-Output "Binaries (v$version) -> $binDir"

# Optional native messaging host registration (Chrome/Edge, Firefox with -Firefox)
if ($ExtensionId) {
    $hostArgs = @{
        ExtensionId = $ExtensionId
        HostPath    = "$binDir\od-nmh.exe"
    }
    if ($Firefox) { $hostArgs.Firefox = $true }
    & "$PSScriptRoot\install-host.ps1" @hostArgs
} else {
    Write-Output "Skipping browser native messaging (pass -ExtensionId <id> to enable)."
}

# Start Menu shortcut when GUI is installed
if (-not $SkipGui -and (Test-Path "$binDir\od-app.exe")) {
    $shortcutPath = "$env:APPDATA\Microsoft\Windows\Start Menu\Programs\Open Downloader.lnk"
    $wsh = New-Object -ComObject WScript.Shell
    $shortcut = $wsh.CreateShortcut($shortcutPath)
    $shortcut.TargetPath = "$binDir\od-app.exe"
    $shortcut.WorkingDirectory = $binDir
    $shortcut.Save()
    Write-Output "Shortcut -> $shortcutPath"
}

# User PATH (idempotent)
$userPath = [Environment]::GetEnvironmentVariable("Path", "User")
if (($userPath -split ";") -notcontains $binDir) {
    [Environment]::SetEnvironmentVariable(
        "Path", ("$userPath".TrimEnd(";") + ";$binDir").TrimStart(";"), "User"
    )
    Write-Output "Added $binDir to user PATH (applies to new terminals)"
}

# Auto-startup configuration
switch ($AutoStartMode) {
    "Daemon" {
        $templatePath = Join-Path $root "deploy\windows\open-downloader-task.xml"
        $xml = (Get-Content $templatePath -Raw) `
            -replace '\{\{BIN_PATH\}\}', "$binDir\od.exe" `
            -replace '\{\{WORK_DIR\}\}', $binDir
        $tmpXml = Join-Path $env:TEMP "open-downloader-task.xml"
        [System.IO.File]::WriteAllText($tmpXml, $xml, [System.Text.UnicodeEncoding]::new($false, $true))
        try {
            & schtasks /Create /TN "OpenDownloaderDaemon" /XML $tmpXml /F | Out-Null
            if ($LASTEXITCODE -eq 0) {
                Write-Output "Auto-start enabled: Windows Task Scheduler 'OpenDownloaderDaemon' (silent background daemon)"
            } else {
                Write-Warning "schtasks failed; falling back to HKCU Run key"
                Set-ItemProperty -Path "HKCU:\Software\Microsoft\Windows\CurrentVersion\Run" `
                    -Name "OpenDownloader" -Value "`"$binDir\od.exe`" serve"
            }
        } finally {
            Remove-Item $tmpXml -Force -ErrorAction SilentlyContinue
        }
    }
    "App" {
        if (-not (Test-Path "$binDir\od-app.exe")) {
            throw "Cannot enable App autostart with -SkipGui"
        }
        Set-ItemProperty -Path "HKCU:\Software\Microsoft\Windows\CurrentVersion\Run" `
            -Name "OpenDownloader" -Value "`"$binDir\od-app.exe`""
        Write-Output "Auto-start enabled: HKCU Run -> od-app.exe"
    }
    "None" {
        Write-Output "Auto-start skipped (-AutoStartMode None). Run 'od autostart enable' anytime."
    }
}

Write-Output "Done (v$version). Run 'od --version' or 'od serve' in a new terminal."
