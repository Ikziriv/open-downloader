#!/usr/bin/env bash
# Clean uninstaller for Open Downloader on Linux and macOS.
# Stops and removes auto-startup services, binaries, and native messaging hosts.
# User downloads are untouched.

set -euo pipefail

OS="$(uname -s)"
PREFIX="${HOME}/.local"
HOST_NAME="com.opendownloader.nmh"

while [[ $# -gt 0 ]]; do
    case "$1" in
        --prefix) PREFIX="$2"; shift 2 ;;
        *) shift ;;
    esac
done

BIN_DIR="${PREFIX}/bin"

if [[ "${OS}" == "Darwin" ]]; then
    PLIST_PATH="${HOME}/Library/LaunchAgents/com.opendownloader.daemon.plist"
    launchctl bootout "gui/$(id -u)" "${PLIST_PATH}" 2>/dev/null \
        || launchctl unload -w "${PLIST_PATH}" 2>/dev/null || true
    rm -f "${PLIST_PATH}"
    rm -rf "${HOME}/Library/Application Support/OpenDownloader"
    rm -f "${HOME}/Library/Application Support/Google/Chrome/NativeMessagingHosts/${HOST_NAME}.json"
    rm -f "${HOME}/Library/Application Support/Microsoft Edge/NativeMessagingHosts/${HOST_NAME}.json"
elif [[ "${OS}" == "Linux" ]]; then
    if command -v systemctl >/dev/null 2>&1; then
        systemctl --user disable --now open-downloader.service 2>/dev/null || true
    fi
    rm -f "${XDG_CONFIG_HOME:-${HOME}/.config}/systemd/user/open-downloader.service"
    rm -f "${XDG_CONFIG_HOME:-${HOME}/.config}/autostart/open-downloader.desktop"
    if command -v systemctl >/dev/null 2>&1; then
        systemctl --user daemon-reload 2>/dev/null || true
    fi
    rm -rf "${XDG_DATA_HOME:-${HOME}/.local/share}/open-downloader"
    rm -f "${HOME}/.config/google-chrome/NativeMessagingHosts/${HOST_NAME}.json"
    rm -f "${HOME}/.config/chromium/NativeMessagingHosts/${HOST_NAME}.json"
    rm -f "${HOME}/.config/microsoft-edge/NativeMessagingHosts/${HOST_NAME}.json"
fi

rm -f "${BIN_DIR}/od" "${BIN_DIR}/od-nmh"
echo "Open Downloader removed cleanly. Downloads folder untouched."
