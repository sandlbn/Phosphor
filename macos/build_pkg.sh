#!/usr/bin/env bash
# ─────────────────────────────────────────────────────────────────────────────
#  build_pkg.sh — Create a macOS installer package (.pkg)
# ─────────────────────────────────────────────────────────────────────────────
#
#  Creates a .pkg that:
#    1. Installs Phosphor.app to /Applications
#    2. Installs the LaunchDaemon plist (postinstall script)
#    3. Starts the usbsid-bridge daemon
#
#  Usage:
#    # Build the .app bundle first:
#    ./macos/build_bundle.sh --sign "Developer ID Application: ..."
#
#    # Then create the .pkg (auto-detects installer cert from keychain):
#    ./macos/build_pkg.sh
#
#    # Or specify the installer identity explicitly:
#    ./macos/build_pkg.sh --sign "Developer ID Installer: Your Name (TEAMID)"
#
#    # With notarization:
#    ./macos/build_pkg.sh --sign "Developer ID Installer: ..." --notarize
#
#  ⚠️  IMPORTANT: .pkg files require a "Developer ID Installer" certificate,
#     NOT "Developer ID Application".  These are separate cert types.
#
#     To create one:
#       1. https://developer.apple.com/account/resources/certificates/list
#       2. Click "+" → "Developer ID Installer"
#       3. Follow the CSR steps → download → double-click to install
#
#     Or in Xcode:
#       Settings → Accounts → Manage Certificates → "+" → Developer ID Installer
#
#  Environment variables:
#    MACOS_INSTALLER_IDENTITY  — productbuild signing identity (overridden by --sign)
#    MACOS_TEAM_ID             — Apple team ID for notarization
#    MACOS_APPLE_ID            — Apple ID for notarization
#    MACOS_APP_PASSWORD        — App-specific password for notarization
# ─────────────────────────────────────────────────────────────────────────────
set -euo pipefail

# Prevent macOS from creating ._* resource fork files during cp
export COPYFILE_DISABLE=1

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$PROJECT_DIR"

# ── Defaults ─────────────────────────────────────────────────────────────────
VERSION=$(grep '^version' Cargo.toml | head -1 | sed 's/.*"\(.*\)"/\1/')
BUNDLE_DIR="target/macos-bundle/Phosphor.app"
BUILD_DIR="target/macos-pkg"
INSTALLER_IDENTITY="${MACOS_INSTALLER_IDENTITY:-}"
NOTARIZE=false

# ── Parse args ───────────────────────────────────────────────────────────────
while [[ $# -gt 0 ]]; do
    case "$1" in
        --sign)     INSTALLER_IDENTITY="$2"; shift 2 ;;
        --notarize) NOTARIZE=true;           shift   ;;
        --bundle)   BUNDLE_DIR="$2";         shift 2 ;;
        *) echo "Unknown option: $1"; exit 1         ;;
    esac
done

if [[ ! -d "$BUNDLE_DIR" ]]; then
    echo "Error: $BUNDLE_DIR not found."
    echo "Build the app bundle first: ./macos/build_bundle.sh"
    exit 1
fi

# Verify the bundle actually has an executable
if [[ ! -x "$BUNDLE_DIR/Contents/MacOS/phosphor" ]]; then
    echo "Error: $BUNDLE_DIR/Contents/MacOS/phosphor not found or not executable."
    exit 1
fi

# ── Validate / auto-detect signing identity ──────────────────────────────────
if [[ -n "$INSTALLER_IDENTITY" ]]; then
    # Catch the most common mistake: passing an Application cert
    if [[ "$INSTALLER_IDENTITY" == *"Developer ID Application"* ]]; then
        echo ""
        echo "╔══════════════════════════════════════════════════════════════╗"
        echo "║  ERROR: Wrong certificate type!                             "
        echo "║                                                             "
        echo "║  You passed: Developer ID Application                       "
        echo "║  .pkg needs: Developer ID Installer                         "
        echo "║                                                             "
        echo "║  These are separate certificates. To create one:            "
        echo "║    https://developer.apple.com/account/resources/certificates"
        echo "║    → '+' → 'Developer ID Installer'                         "
        echo "║                                                             "
        echo "║  Or in Xcode:                                               "
        echo "║    Settings → Accounts → Manage Certificates                "
        echo "║    → '+' → Developer ID Installer                           "
        echo "╚══════════════════════════════════════════════════════════════╝"
        echo ""
        echo "Building unsigned .pkg instead ..."
        INSTALLER_IDENTITY=""
    fi
fi

# Auto-detect from keychain if not provided
if [[ -z "$INSTALLER_IDENTITY" ]]; then
    echo "==> Searching keychain for Developer ID Installer certificate ..."
    DETECTED=$(security find-identity -v -p basic 2>/dev/null \
        | grep "Developer ID Installer" \
        | head -1 \
        | sed 's/.*"\(.*\)"/\1/' || true)

    if [[ -n "$DETECTED" ]]; then
        INSTALLER_IDENTITY="$DETECTED"
        echo "    ✓ Found: $INSTALLER_IDENTITY"
    else
        echo "    ⚠ No Developer ID Installer certificate found in keychain."
        echo "      The .pkg will be built unsigned."
        echo ""
    fi
fi

SIGN_LABEL="${INSTALLER_IDENTITY:-unsigned}"
echo "╔══════════════════════════════════════════════════════════════╗"
echo "║  Building Phosphor-${VERSION}.pkg"
echo "║  Bundle: $BUNDLE_DIR"
echo "║  Sign:   $SIGN_LABEL"
echo "╚══════════════════════════════════════════════════════════════╝"

# ── Clean workspace ──────────────────────────────────────────────────────────
rm -rf "$BUILD_DIR"
mkdir -p "$BUILD_DIR/staging/scripts"
mkdir -p "$BUILD_DIR/staging/payload"
mkdir -p "$BUILD_DIR/staging/resources"
mkdir -p "$BUILD_DIR/out"

# ── Staging ──────────────────────────────────────────────────────────────────
echo ""
echo "==> Staging payload ..."
cp -R "$BUNDLE_DIR" "$BUILD_DIR/staging/payload/Phosphor.app"

# Clean macOS resource forks and metadata from the payload.
# These ._* files confuse pkgbuild and can cause phantom installs where
# the receipt exists but files don't actually land on disk.
echo "==> Cleaning resource forks ..."
find "$BUILD_DIR/staging/payload" -name '._*' -delete 2>/dev/null || true
find "$BUILD_DIR/staging/payload" -name '.DS_Store' -delete 2>/dev/null || true
dot_clean "$BUILD_DIR/staging/payload" 2>/dev/null || true

# Verify payload is sane
APP_COUNT=$(find "$BUILD_DIR/staging/payload/Phosphor.app" -type f | wc -l | tr -d ' ')
echo "    Payload contains $APP_COUNT files"
if [[ "$APP_COUNT" -lt 3 ]]; then
    echo "Error: payload looks empty — something went wrong with the bundle."
    exit 1
fi

# ── postinstall script ───────────────────────────────────────────────────────
cat > "$BUILD_DIR/staging/scripts/postinstall" << 'POSTINSTALL'
#!/bin/bash
set -e

LABEL="com.phosphor.usbsid-bridge"
PLIST="/Library/LaunchDaemons/$LABEL.plist"
BRIDGE="/Applications/Phosphor.app/Contents/Helpers/usbsid-bridge"
SOCKET="/tmp/usbsid-bridge.sock"

# Stop any existing daemon
/bin/launchctl bootout system/"$LABEL" 2>/dev/null || \
    /bin/launchctl unload "$PLIST" 2>/dev/null || true
killall usbsid-bridge 2>/dev/null || true
rm -f "$SOCKET"

# Write the LaunchDaemon plist
cat > "$PLIST" << PLISTEOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>$LABEL</string>
    <key>ProgramArguments</key>
    <array>
        <string>$BRIDGE</string>
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
PLISTEOF

chown root:wheel "$PLIST"
chmod 644 "$PLIST"

# Start the daemon
/bin/launchctl bootstrap system "$PLIST" 2>/dev/null || \
    /bin/launchctl load "$PLIST"

# Clean up any legacy install
rm -f /usr/local/bin/usbsid-bridge 2>/dev/null || true

exit 0
POSTINSTALL
chmod +x "$BUILD_DIR/staging/scripts/postinstall"

# ── preinstall script ────────────────────────────────────────────────────────
cat > "$BUILD_DIR/staging/scripts/preinstall" << 'PREINSTALL'
#!/bin/bash
LABEL="com.phosphor.usbsid-bridge"
PLIST="/Library/LaunchDaemons/$LABEL.plist"

/bin/launchctl bootout system/"$LABEL" 2>/dev/null || \
    /bin/launchctl unload "$PLIST" 2>/dev/null || true
killall usbsid-bridge 2>/dev/null || true
rm -f /tmp/usbsid-bridge.sock

exit 0
PREINSTALL
chmod +x "$BUILD_DIR/staging/scripts/preinstall"

# ── Component package ────────────────────────────────────────────────────────
echo "==> Building component package ..."

# Component pkg stays in staging/ — NOT in the final output
COMPONENT_PKG="$BUILD_DIR/staging/PhosphorComponent.pkg"

# CRITICAL: Create a component plist that disables bundle relocation.
# Without this, macOS Installer will search the disk for any existing bundle
# with the same CFBundleIdentifier and "relocate" the install there — e.g.
# updating the build directory copy instead of installing to /Applications.
cat > "$BUILD_DIR/staging/component.plist" << 'CPLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<array>
    <dict>
        <key>BundleHasStrictIdentifier</key>
        <true/>
        <key>BundleIsRelocatable</key>
        <false/>
        <key>BundleIsVersionChecked</key>
        <true/>
        <key>BundleOverwriteAction</key>
        <string>upgrade</string>
        <key>RootRelativeBundlePath</key>
        <string>Phosphor.app</string>
    </dict>
</array>
</plist>
CPLIST

pkgbuild \
    --root "$BUILD_DIR/staging/payload" \
    --install-location "/Applications" \
    --scripts "$BUILD_DIR/staging/scripts" \
    --component-plist "$BUILD_DIR/staging/component.plist" \
    --identifier "com.phosphor.player" \
    --version "$VERSION" \
    "$COMPONENT_PKG"

# ── Resources for the installer UI ──────────────────────────────────────────
cat > "$BUILD_DIR/staging/resources/welcome.html" << 'WELCOME'
<html>
<head><style>
  body { font-family: -apple-system, Helvetica Neue, sans-serif; padding: 20px; }
  h1 { font-size: 22px; }
  p { font-size: 14px; line-height: 1.5; }
  ul { font-size: 14px; }
</style></head>
<body>
<h1>Phosphor</h1>
<p>A SID music player for <b>USBSID-Pico</b> hardware, software emulation,
and Ultimate 64 network playback.</p>
<p>This installer will:</p>
<ul>
  <li>Install <b>Phosphor.app</b> to /Applications</li>
  <li>Set up the USB bridge daemon for USBSID-Pico hardware access</li>
</ul>
</body></html>
WELCOME

cat > "$BUILD_DIR/staging/resources/conclusion.html" << 'CONCLUSION'
<html>
<head><style>
  body { font-family: -apple-system, Helvetica Neue, sans-serif; padding: 20px; }
  h1 { font-size: 22px; color: #2d8a4e; }
  p { font-size: 14px; line-height: 1.5; }
</style></head>
<body>
<h1>✓ Installation Complete</h1>
<p><b>Phosphor</b> has been installed to /Applications.</p>
<p>The USB bridge daemon is running and will start automatically on boot.</p>
<p>Open <b>Phosphor</b> from your Applications folder or Launchpad.</p>
</body></html>
CONCLUSION

# ── Distribution XML ────────────────────────────────────────────────────────
# Build the background line only if the image exists
BACKGROUND_LINE=""
if [[ -f "assets/phosphor.png" ]]; then
    cp "assets/phosphor.png" "$BUILD_DIR/staging/resources/background.png"
    BACKGROUND_LINE='    <background file="background.png" alignment="bottomleft" scaling="proportional"/>'
fi

cat > "$BUILD_DIR/staging/distribution.xml" << DISTXML
<?xml version="1.0" encoding="utf-8"?>
<installer-gui-script minSpecVersion="2">
    <title>Phosphor ${VERSION}</title>
    <welcome file="welcome.html"/>
    <conclusion file="conclusion.html"/>
${BACKGROUND_LINE}
    <options customize="never"/>
    <domains enable_localSystem="true"/>
    <os-version min="11.0"/>
    <pkg-ref id="com.phosphor.player"/>
    <choices-outline>
        <line choice="default">
            <line choice="com.phosphor.player"/>
        </line>
    </choices-outline>
    <choice id="default"/>
    <choice id="com.phosphor.player" visible="false">
        <pkg-ref id="com.phosphor.player"/>
    </choice>
    <pkg-ref id="com.phosphor.player" version="${VERSION}" installKBytes="$(du -sk "$BUILD_DIR/staging/payload" | cut -f1)">PhosphorComponent.pkg</pkg-ref>
</installer-gui-script>
DISTXML

# ── productbuild (final .pkg) ───────────────────────────────────────────────
echo "==> Building distribution package ..."

FINAL_PKG="$BUILD_DIR/out/Phosphor-${VERSION}.pkg"

SIGN_ARGS=()
if [[ -n "$INSTALLER_IDENTITY" ]]; then
    SIGN_ARGS=(--sign "$INSTALLER_IDENTITY")
    echo "    Signing with: $INSTALLER_IDENTITY"
else
    echo "    Building unsigned (no installer certificate)"
fi

productbuild \
    --distribution "$BUILD_DIR/staging/distribution.xml" \
    --package-path "$BUILD_DIR/staging" \
    --resources "$BUILD_DIR/staging/resources" \
    "${SIGN_ARGS[@]}" \
    "$FINAL_PKG"

echo "    ✓ $(basename "$FINAL_PKG")"

# ── Verify the package ──────────────────────────────────────────────────────
echo ""
echo "==> Verifying package ..."

# Check the pkg contains the expected payload
pkgutil --payload-files "$COMPONENT_PKG" | head -5
echo "    ..."

if [[ -n "$INSTALLER_IDENTITY" ]]; then
    pkgutil --check-signature "$FINAL_PKG" | head -5
else
    echo "    (unsigned — skipping signature check)"
fi

# ── Clean up intermediate artifacts ──────────────────────────────────────────
# Only the final .pkg should exist in out/ — nothing else
echo ""
echo "==> Cleaning up staging files ..."
rm -rf "$BUILD_DIR/staging"

# ── Notarize ─────────────────────────────────────────────────────────────────
if $NOTARIZE; then
    echo ""
    echo "==> Notarizing ..."

    TEAM_ID="${MACOS_TEAM_ID:?Set MACOS_TEAM_ID for notarization}"
    APPLE_ID="${MACOS_APPLE_ID:?Set MACOS_APPLE_ID for notarization}"
    APP_PASS="${MACOS_APP_PASSWORD:?Set MACOS_APP_PASSWORD for notarization}"

    xcrun notarytool submit "$FINAL_PKG" \
        --apple-id "$APPLE_ID" \
        --team-id "$TEAM_ID" \
        --password "$APP_PASS" \
        --wait

    xcrun stapler staple "$FINAL_PKG"
    echo "    ✓ Notarization complete"
fi

# ── Done ─────────────────────────────────────────────────────────────────────
echo ""
echo "╔══════════════════════════════════════════════════════════════╗"
echo "║  ✓ Package: $FINAL_PKG"
echo "║"
echo "║  Users double-click the .pkg → authenticate once →"
echo "║  Phosphor + daemon installed and running."
echo "║"
echo "║  To verify after install:"
echo "║    pkgutil --pkg-info com.phosphor.player"
echo "║    ls /Applications/Phosphor.app"
echo "╚══════════════════════════════════════════════════════════════╝"
