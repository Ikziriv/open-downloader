# Removes what install.ps1 created: auto-startup tasks/registry entries,
# binaries, Start Menu shortcut, PATH entry, and native-messaging registrations.
# The user's Downloads folder is untouched.
param(
    [string]$InstallDir = "$env:LOCALAPPDATA\OpenDownloader",
    [string]$HostName = "com.opendownloader.nmh"
)

$ErrorActionPreference = "Stop"
$binDir = Join-Path $InstallDir "bin"

# Stop running processes
Get-Process od, od-app, od-nmh -ErrorAction SilentlyContinue | Stop-Process -Force

# Remove auto-startup entries (Task Scheduler + HKCU Run)
& schtasks /Delete /TN "OpenDownloaderDaemon" /F 2>$null | Out-Null
Remove-ItemProperty -Path "HKCU:\Software\Microsoft\Windows\CurrentVersion\Run" `
    -Name "OpenDownloader" -ErrorAction SilentlyContinue

# Remove user PATH entry
$userPath = [Environment]::GetEnvironmentVariable("Path", "User")
if ($userPath) {
    $kept = ($userPath -split ";" | Where-Object { $_ -and $_ -ne $binDir }) -join ";"
    [Environment]::SetEnvironmentVariable("Path", $kept, "User")
}

# Remove Start Menu shortcut
Remove-Item "$env:APPDATA\Microsoft\Windows\Start Menu\Programs\Open Downloader.lnk" `
    -Force -ErrorAction SilentlyContinue

# Remove browser native messaging registry keys
foreach ($key in
    "HKCU:\Software\Google\Chrome\NativeMessagingHosts\$HostName",
    "HKCU:\Software\Microsoft\Edge\NativeMessagingHosts\$HostName",
    "HKCU:\Software\Mozilla\NativeMessagingHosts\$HostName") {
    Remove-Item $key -Recurse -Force -ErrorAction SilentlyContinue
}

Remove-Item $InstallDir -Recurse -Force -ErrorAction SilentlyContinue
Write-Output "Open Downloader removed cleanly (auto-start, binaries, PATH, and native messaging). Downloads folder untouched."
