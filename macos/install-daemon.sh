#!/usr/bin/env bash
# ─────────────────────────────────────────────────────────────────────────────
#  install-daemon.sh — Install / update the usbsid-bridge LaunchDaemon
# ─────────────────────────────────────────────────────────────────────────────
#
#  This script is bundled inside Phosphor.app/Contents/Resources/.
#  It installs the launchd plist that keeps the USB bridge daemon running.
#
#  The daemon binary lives inside the app bundle at:
#    /Applications/Phosphor.app/Contents/Helpers/usbsid-bridge
#
#  Usage:
#    # From the app bundle:
#    /Applications/Phosphor.app/Contents/Resources/install-daemon.sh
#
#    # Or if running from the source tree:
#    ./macos/install-daemon.sh [--app-path /path/to/Phosphor.app]
# ─────────────────────────────────────────────────────────────────────────────
set -euo pipefail

PLIST_LABEL="com.phosphor.usbsid-bridge"
PLIST_DST="/Library/LaunchDaemons/$PLIST_LABEL.plist"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

# ── Locate the app bundle ───────────────────────────────────────────────────
APP_PATH=""
while [[ $# -gt 0 ]]; do
    case "$1" in
        --app-path) APP_PATH="$2"; shift 2 ;;
        *) echo "Unknown option: $1"; exit 1 ;;
    esac
done

if [[ -z "$APP_PATH" ]]; then
    # Auto-detect: are we inside a .app bundle?
    if [[ "$SCRIPT_DIR" == */Contents/Resources ]]; then
        APP_PATH="$(cd "$SCRIPT_DIR/../.." && pwd)"
    elif [[ -d "/Applications/Phosphor.app" ]]; then
        APP_PATH="/Applications/Phosphor.app"
    else
        echo "Error: Cannot locate Phosphor.app"
        echo "Either run this script from inside the bundle, install the app"
        echo "to /Applications, or use --app-path /path/to/Phosphor.app"
        exit 1
    fi
fi

BRIDGE_BIN="$APP_PATH/Contents/Helpers/usbsid-bridge"
if [[ ! -x "$BRIDGE_BIN" ]]; then
    echo "Error: Bridge binary not found at $BRIDGE_BIN"
    exit 1
fi

echo "╔══════════════════════════════════════════════════════════════╗"
echo "║  Installing usbsid-bridge LaunchDaemon                      "
echo "║  App:    $APP_PATH"
echo "║  Bridge: $BRIDGE_BIN"
echo "╚══════════════════════════════════════════════════════════════╝"
echo ""

# ── Stop existing daemon ─────────────────────────────────────────────────────
echo "==> Stopping existing daemon (if any) ..."
sudo launchctl bootout system/"$PLIST_LABEL" 2>/dev/null || true
sudo killall usbsid-bridge 2>/dev/null || true
sudo rm -f /tmp/usbsid-bridge.sock

# ── Write the plist (with correct path to bridge binary) ─────────────────────
echo "==> Installing LaunchDaemon plist ..."
sudo tee "$PLIST_DST" > /dev/null <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>$PLIST_LABEL</string>
    <key>ProgramArguments</key>
    <array>
        <string>$BRIDGE_BIN</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>StandardErrorPath</key>
    <string>/tmp/usbsid-bridge.log</string>
    <key>StandardOutPath</key>
    <string>/tmp/usbsid-bridge.log</string>
</dict>
</plist>
PLIST

sudo chown root:wheel "$PLIST_DST"
sudo chmod 644 "$PLIST_DST"

# ── Start daemon ─────────────────────────────────────────────────────────────
echo "==> Starting daemon ..."
sudo launchctl bootstrap system "$PLIST_DST"

# ── Verify ───────────────────────────────────────────────────────────────────
echo "==> Verifying ..."
sleep 1.5
if [[ -S /tmp/usbsid-bridge.sock ]]; then
    echo ""
    echo "✓ Bridge daemon running — socket ready at /tmp/usbsid-bridge.sock"
    echo "  Logs: tail -f /tmp/usbsid-bridge.log"
else
    echo ""
    echo "✗ Socket not found after 1.5 s — check: tail -f /tmp/usbsid-bridge.log"
    echo "  The daemon may still be starting. If your USBSID-Pico isn't"
    echo "  connected, the socket won't appear until the first client connects."
    exit 1
fi
