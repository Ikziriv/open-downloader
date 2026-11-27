# Registers the Open Downloader native messaging host with Chrome, Edge
# and (optionally) Firefox.
#
# Usage:
#   1. Build:  cargo build --release
#   2. Load the extension -> chrome://extensions -> "Load unpacked" -> pick extension/
#   3. Copy the extension ID shown on the card
#   4. Run:   .\scripts\install-host.ps1 -ExtensionId abcdefghijklmnopqrstuvwxyzabcdef
#      For Firefox additionally pass: -Firefox
param(
    [Parameter(Mandatory = $true)]
    [string]$ExtensionId,

    [switch]$Firefox,
    [string]$FirefoxExtensionId = "opendownloader@localhost",

    [string]$HostName = "com.opendownloader.nmh",
    [string]$HostPath = "$PSScriptRoot\..\target\release\od-nmh.exe"
)

$resolved = Resolve-Path $HostPath -ErrorAction Stop
$hostDir = Split-Path $resolved.Path
foreach ($bin in "od.exe", "od-app.exe") {
    if (-not (Test-Path (Join-Path $hostDir $bin))) {
        Write-Warning "$bin not found next to od-nmh.exe — the app-fallback path will not work. Run 'cargo build --release' first."
    }
}

$manifestDir = "$env:LOCALAPPDATA\OpenDownloader\native-messaging"
New-Item -ItemType Directory -Force $manifestDir | Out-Null

function Write-Manifest($path, $allowed) {
    $manifest = @{
        name        = $HostName
        description = "Open Downloader native messaging host"
        path        = $resolved.Path
        type        = "stdio"
    } + $allowed | ConvertTo-Json
    # Chrome/Firefox reject manifests with a UTF-8 BOM — write without one.
    [System.IO.File]::WriteAllText($path, $manifest, [System.Text.UTF8Encoding]::new($false))
}

$chromeManifest = Join-Path $manifestDir "$HostName.json"
Write-Manifest $chromeManifest @{ allowed_origins = @("chrome-extension://$ExtensionId/") }
foreach ($browser in "Google\Chrome", "Microsoft\Edge") {
    $key = "HKCU:\Software\$browser\NativeMessagingHosts\$HostName"
    New-Item -Path $key -Force | Out-Null
    Set-ItemProperty -Path $key -Name "(Default)" -Value $chromeManifest
}
Write-Output "Registered $HostName (Chrome/Edge): $chromeManifest"

if ($Firefox) {
    # Firefox uses allowed_extensions and its own registry hive + manifest.
    $ffManifest = Join-Path $manifestDir "$HostName.firefox.json"
    Write-Manifest $ffManifest @{ allowed_extensions = @($FirefoxExtensionId) }
    $key = "HKCU:\Software\Mozilla\NativeMessagingHosts\$HostName"
    New-Item -Path $key -Force | Out-Null
    Set-ItemProperty -Path $key -Name "(Default)" -Value $ffManifest
    Write-Output "Registered $HostName (Firefox): $ffManifest"
    Write-Output "Load extension/manifest.firefox.json via about:debugging"
}

Write-Output "Reload the extension, then any download should be captured by Open Downloader."
