#!/usr/bin/env bash
# Cross-platform installer for Open Downloader on Linux and macOS.
#
# Installs `od` and `od-nmh`, stamps version.txt, optionally registers browser
# native messaging, and enables native user-level auto-startup:
#   - Linux : systemd --user service (with XDG .desktop autostart fallback)
#   - macOS : LaunchAgent plist in ~/Library/LaunchAgents
#
# Usage:
#   ./scripts/install.sh [--skip-build] [--no-autostart] [--prefix ~/.local]
#                        [--extension-id <chrome-ext-id>]

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"

OS="$(uname -s)"
ARCH="$(uname -m)"

SKIP_BUILD=0
AUTOSTART=1
EXTENSION_ID=""
PREFIX="${HOME}/.local"

while [[ $# -gt 0 ]]; do
    case "$1" in
        --skip-build) SKIP_BUILD=1; shift ;;
        --no-autostart) AUTOSTART=0; shift ;;
        --prefix) PREFIX="$2"; shift 2 ;;
        --extension-id) EXTENSION_ID="$2"; shift 2 ;;
        -h|--help)
            sed -n '2,14p' "$0"
            exit 0
            ;;
        *)
            echo "Unknown option: $1" >&2
            exit 2
            ;;
    esac
done

VERSION="$(awk -F'"' '/^version[[:space:]]*=/{print $2; exit}' "${ROOT_DIR}/Cargo.toml")"
VERSION="${VERSION:-0.0.0}"

echo "Installing Open Downloader v${VERSION} (${OS}/${ARCH})..."

if [[ "${SKIP_BUILD}" -eq 0 ]]; then
    cargo build --release --manifest-path "${ROOT_DIR}/Cargo.toml" -p od-cli -p od-nmh
fi

BIN_DIR="${PREFIX}/bin"
mkdir -p "${BIN_DIR}"

for bin in od od-nmh; do
    SRC="${ROOT_DIR}/target/release/${bin}"
    if [[ ! -f "${SRC}" ]]; then
        echo "error: ${SRC} not found — run 'cargo build --release' first" >&2
        exit 1
    fi
    install -m 755 "${SRC}" "${BIN_DIR}/${bin}"
done

if [[ "${OS}" == "Darwin" ]]; then
    APP_DATA_DIR="${HOME}/Library/Application Support/OpenDownloader"
    LOG_DIR="${HOME}/Library/Logs/OpenDownloader"
else
    APP_DATA_DIR="${XDG_DATA_HOME:-${HOME}/.local/share}/open-downloader"
    LOG_DIR="${XDG_STATE_HOME:-${HOME}/.local/state}/open-downloader"
fi

mkdir -p "${APP_DATA_DIR}" "${LOG_DIR}"
printf '%s' "${VERSION}" > "${APP_DATA_DIR}/version.txt"
echo "Binaries (v${VERSION}) -> ${BIN_DIR}"

# Optional browser native messaging registration
if [[ -n "${EXTENSION_ID}" ]]; then
    HOST_NAME="com.opendownloader.nmh"
    MANIFEST_JSON=$(cat <<EOF
{
  "name": "${HOST_NAME}",
  "description": "Open Downloader native messaging host",
  "path": "${BIN_DIR}/od-nmh",
  "type": "stdio",
  "allowed_origins": ["chrome-extension://${EXTENSION_ID}/"]
}
EOF
)
    if [[ "${OS}" == "Darwin" ]]; then
        NMH_DIRS=(
            "${HOME}/Library/Application Support/Google/Chrome/NativeMessagingHosts"
            "${HOME}/Library/Application Support/Microsoft Edge/NativeMessagingHosts"
        )
    else
        NMH_DIRS=(
            "${HOME}/.config/google-chrome/NativeMessagingHosts"
            "${HOME}/.config/chromium/NativeMessagingHosts"
            "${HOME}/.config/microsoft-edge/NativeMessagingHosts"
        )
    fi
    for dir in "${NMH_DIRS[@]}"; do
        mkdir -p "${dir}"
        printf '%s\n' "${MANIFEST_JSON}" > "${dir}/${HOST_NAME}.json"
    done
    echo "Registered native messaging host (${HOST_NAME})"
fi

# Configure OS-native auto-startup
if [[ "${AUTOSTART}" -eq 1 ]]; then
    if [[ "${OS}" == "Darwin" ]]; then
        PLIST_DIR="${HOME}/Library/LaunchAgents"
        PLIST_PATH="${PLIST_DIR}/com.opendownloader.daemon.plist"
        mkdir -p "${PLIST_DIR}"
        sed \
            -e "s|{{BIN_PATH}}|${BIN_DIR}/od|g" \
            -e "s|{{LOG_DIR}}|${LOG_DIR}|g" \
            "${ROOT_DIR}/deploy/macos/com.opendownloader.daemon.plist" > "${PLIST_PATH}"
        launchctl bootout "gui/$(id -u)" "${PLIST_PATH}" 2>/dev/null || true
        launchctl bootstrap "gui/$(id -u)" "${PLIST_PATH}" 2>/dev/null \
            || launchctl load -w "${PLIST_PATH}"
        echo "Auto-start enabled (macOS LaunchAgent): ${PLIST_PATH}"
    elif [[ "${OS}" == "Linux" ]]; then
        UNIT_DIR="${XDG_CONFIG_HOME:-${HOME}/.config}/systemd/user"
        UNIT_PATH="${UNIT_DIR}/open-downloader.service"
        mkdir -p "${UNIT_DIR}"
        sed \
            -e "s|{{BIN_PATH}}|${BIN_DIR}/od|g" \
            -e "s|{{WORK_DIR}}|${BIN_DIR}|g" \
            "${ROOT_DIR}/deploy/linux/open-downloader.service" > "${UNIT_PATH}"

        if command -v systemctl >/dev/null 2>&1 && systemctl --user daemon-reload 2>/dev/null; then
            systemctl --user enable --now open-downloader.service
            echo "Auto-start enabled (systemd --user): ${UNIT_PATH}"
        else
            XDG_DIR="${XDG_CONFIG_HOME:-${HOME}/.config}/autostart"
            mkdir -p "${XDG_DIR}"
            sed -e "s|{{BIN_PATH}}|${BIN_DIR}/od|g" \
                "${ROOT_DIR}/deploy/linux/open-downloader.desktop" > "${XDG_DIR}/open-downloader.desktop"
            echo "Auto-start enabled (XDG autostart): ${XDG_DIR}/open-downloader.desktop"
        fi
    else
        echo "Warning: unsupported OS '${OS}' for automatic startup." >&2
    fi
fi

# Verify PATH hint
case ":${PATH}:" in
    *":${BIN_DIR}:"*) ;;
    *)
        echo "Note: add '${BIN_DIR}' to your shell PATH (e.g. export PATH=\"${BIN_DIR}:\$PATH\")."
        ;;
esac

# Quick health probe if daemon was started
if [[ "${AUTOSTART}" -eq 1 ]] && command -v curl >/dev/null 2>&1; then
    sleep 1
    if curl -sf "http://127.0.0.1:19799/api/v1/health" >/dev/null 2>&1; then
        echo "Daemon health check: OK (http://127.0.0.1:19799/api/v1/health)"
    fi
fi

echo "Done (v${VERSION}). Run 'od --version' or 'od autostart status'."
