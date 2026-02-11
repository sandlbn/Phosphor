#!/bin/bash
set -e

PLIST_NAME="com.phosphor.usbsid-bridge"
PLIST_SRC="$PLIST_NAME.plist"
PLIST_DST="/Library/LaunchDaemons/$PLIST_NAME.plist"
BRIDGE_DST="/usr/local/bin/usbsid-bridge"

echo "=== Building Phosphor ==="
cargo build --release
cargo build --release --bin usbsid-bridge

echo "=== Signing phosphor ==="
codesign --force --sign - target/release/phosphor

echo "=== Installing bridge daemon ==="
sudo launchctl unload "$PLIST_DST" 2>/dev/null || true
sudo killall usbsid-bridge 2>/dev/null || true
sudo rm -f /tmp/usbsid-bridge.sock

sudo cp target/release/usbsid-bridge "$BRIDGE_DST"
sudo chown root:wheel "$BRIDGE_DST"
sudo chmod 755 "$BRIDGE_DST"
sudo cp "$PLIST_SRC" "$PLIST_DST"
sudo chown root:wheel "$PLIST_DST"
sudo chmod 644 "$PLIST_DST"

sudo launchctl load "$PLIST_DST"

echo "=== Verifying daemon ==="
sleep 1
if [ -S /tmp/usbsid-bridge.sock ]; then
    echo "✓ Bridge daemon running, socket ready"
else
    echo "✗ Socket not found — check: tail -f /tmp/usbsid-bridge.log"
    exit 1
fi

echo ""
echo "=== Done ==="
echo "Run with: ./target/release/phosphor"
echo "Logs:     tail -f /tmp/usbsid-bridge.log"
