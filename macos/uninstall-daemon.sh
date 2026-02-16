#!/usr/bin/env bash
# ─────────────────────────────────────────────────────────────────────────────
#  uninstall-daemon.sh — Remove the usbsid-bridge LaunchDaemon
# ─────────────────────────────────────────────────────────────────────────────
set -euo pipefail

PLIST_LABEL="com.phosphor.usbsid-bridge"
PLIST_DST="/Library/LaunchDaemons/$PLIST_LABEL.plist"

echo "==> Stopping usbsid-bridge daemon ..."
sudo launchctl bootout system/"$PLIST_LABEL" 2>/dev/null || true
sudo killall usbsid-bridge 2>/dev/null || true
sudo rm -f /tmp/usbsid-bridge.sock

echo "==> Removing plist ..."
sudo rm -f "$PLIST_DST"

# Also clean up legacy install (pre-bundle /usr/local/bin copy)
if [[ -f /usr/local/bin/usbsid-bridge ]]; then
    echo "==> Removing legacy /usr/local/bin/usbsid-bridge ..."
    sudo rm -f /usr/local/bin/usbsid-bridge
fi

echo ""
echo "✓ usbsid-bridge daemon removed."
