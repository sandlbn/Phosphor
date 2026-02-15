#!/bin/bash
# ─────────────────────────────────────────────────────────────────────────────
#  install.sh — Build from source and install (quick non-bundle path)
# ─────────────────────────────────────────────────────────────────────────────
#
#  macOS users: prefer ./macos/build_bundle.sh for a proper .app bundle.
#  This script builds, embeds libusb alongside the bridge binary, signs
#  both, and installs the LaunchDaemon.
# ─────────────────────────────────────────────────────────────────────────────
set -e

PLIST_NAME="com.phosphor.usbsid-bridge"
PLIST_SRC="$PLIST_NAME.plist"
PLIST_DST="/Library/LaunchDaemons/$PLIST_NAME.plist"
BRIDGE_DST="/usr/local/bin/usbsid-bridge"
LIB_DIR="/usr/local/lib/phosphor"

echo "=== Building Phosphor ==="
cargo build --release
cargo build --release --bin usbsid-bridge

echo "=== Embedding dynamic libraries ==="
# The bridge links libusb dynamically. Hardened-runtime codesigning requires
# all loaded dylibs to share the same team ID — Homebrew's libusb is unsigned
# so we must bundle it alongside the binary and rewrite the load path.

mkdir -p "$LIB_DIR"

embed_dylibs() {
    local binary="$1"
    otool -L "$binary" 2>/dev/null | tail -n +2 | while read -r line; do
        local dylib
        dylib=$(echo "$line" | sed 's/^[[:space:]]*//' | sed 's/ (compatibility.*//')
        case "$dylib" in
            /usr/lib/*|/System/*|@rpath/*|@executable_path/*|@loader_path/*) continue ;;
        esac
        local libname real_path
        libname=$(basename "$dylib")
        real_path=$(realpath "$dylib" 2>/dev/null || echo "$dylib")
        if [[ -f "$real_path" ]]; then
            echo "  ✓ $libname  ← $real_path"
            sudo cp -L "$real_path" "$LIB_DIR/$libname"
            sudo chmod 644 "$LIB_DIR/$libname"
            install_name_tool -change "$dylib" "@rpath/$libname" "$binary"
            sudo install_name_tool -id "@rpath/$libname" "$LIB_DIR/$libname" 2>/dev/null || true
        fi
    done
    install_name_tool -add_rpath "$LIB_DIR" "$binary" 2>/dev/null || true
}

# Work on copies so we don't modify the build artifacts
cp target/release/phosphor target/release/phosphor.signed
cp target/release/usbsid-bridge target/release/usbsid-bridge.signed

embed_dylibs target/release/phosphor.signed
embed_dylibs target/release/usbsid-bridge.signed

echo "=== Signing ==="
codesign --force --options runtime \
    --entitlements macos/Phosphor.entitlements \
    --sign - target/release/phosphor.signed

codesign --force --options runtime \
    --entitlements macos/Bridge.entitlements \
    --sign - target/release/usbsid-bridge.signed

# Sign embedded dylibs too
for lib in "$LIB_DIR"/*.dylib; do
    [[ -f "$lib" ]] && sudo codesign --force --options runtime --sign - "$lib"
done

echo "=== Installing bridge daemon ==="
sudo launchctl bootout system/"$PLIST_NAME" 2>/dev/null || \
    sudo launchctl unload "$PLIST_DST" 2>/dev/null || true
sudo killall usbsid-bridge 2>/dev/null || true
sudo rm -f /tmp/usbsid-bridge.sock

sudo cp target/release/usbsid-bridge.signed "$BRIDGE_DST"
sudo chown root:wheel "$BRIDGE_DST"
sudo chmod 755 "$BRIDGE_DST"
sudo cp "$PLIST_SRC" "$PLIST_DST"
sudo chown root:wheel "$PLIST_DST"
sudo chmod 644 "$PLIST_DST"

# Copy the signed phosphor into place
cp target/release/phosphor.signed target/release/phosphor

sudo launchctl bootstrap system "$PLIST_DST" 2>/dev/null || \
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
