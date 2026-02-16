#!/usr/bin/env bash
# ─────────────────────────────────────────────────────────────────────────────
#  build_bundle.sh — Build Phosphor.app macOS bundle
# ─────────────────────────────────────────────────────────────────────────────
#
#  Usage:
#    ./macos/build_bundle.sh                     # ad-hoc signed bundle
#    ./macos/build_bundle.sh --sign "Developer ID Application: ..."
#    ./macos/build_bundle.sh --sign "Developer ID Application: ..." --dmg
#    ./macos/build_bundle.sh --sign "Developer ID Application: ..." --notarize
#
#  Environment variables (for CI):
#    MACOS_SIGN_IDENTITY   — codesign identity (overridden by --sign)
#    MACOS_TEAM_ID         — Apple team ID for notarization
#    MACOS_APPLE_ID        — Apple ID for notarization
#    MACOS_APP_PASSWORD    — App-specific password for notarization
#    MACOS_ARCH            — Target architecture: "x86_64" | "aarch64" | "universal"
#                            (default: current host arch)
# ─────────────────────────────────────────────────────────────────────────────
set -euo pipefail

# Prevent macOS from creating ._* resource fork files during cp
export COPYFILE_DISABLE=1

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$PROJECT_DIR"

# ── Defaults ─────────────────────────────────────────────────────────────────
APP_NAME="Phosphor"
BUNDLE_ID="com.phosphor.player"
VERSION=$(grep '^version' Cargo.toml | head -1 | sed 's/.*"\(.*\)"/\1/')
SIGN_IDENTITY="${MACOS_SIGN_IDENTITY:--}"
CREATE_DMG=false
NOTARIZE=false
ARCH="${MACOS_ARCH:-$(uname -m)}"

# ── Parse args ───────────────────────────────────────────────────────────────
while [[ $# -gt 0 ]]; do
    case "$1" in
        --sign)    SIGN_IDENTITY="$2"; shift 2 ;;
        --dmg)     CREATE_DMG=true;    shift   ;;
        --notarize) NOTARIZE=true;     shift   ;;
        --arch)    ARCH="$2";          shift 2 ;;
        *) echo "Unknown option: $1"; exit 1   ;;
    esac
done

# Map arch names to Rust targets
case "$ARCH" in
    arm64|aarch64)
        RUST_TARGETS=("aarch64-apple-darwin")
        ;;
    x86_64)
        RUST_TARGETS=("x86_64-apple-darwin")
        ;;
    universal)
        RUST_TARGETS=("aarch64-apple-darwin" "x86_64-apple-darwin")
        ;;
    *)
        echo "Unsupported arch: $ARCH"
        exit 1
        ;;
esac

echo "╔══════════════════════════════════════════════════════════════╗"
echo "║  Building $APP_NAME v$VERSION for macOS ($ARCH)"
echo "║  Sign identity: $SIGN_IDENTITY"
echo "╚══════════════════════════════════════════════════════════════╝"

# ── Build ────────────────────────────────────────────────────────────────────
BUILD_DIR="$PROJECT_DIR/target/macos-bundle"
BUNDLE_DIR="$BUILD_DIR/$APP_NAME.app"
rm -rf "$BUNDLE_DIR"

for TARGET in "${RUST_TARGETS[@]}"; do
    echo ""
    echo "==> Building for $TARGET ..."
    cargo build --release --target "$TARGET"
    cargo build --release --target "$TARGET" --bin usbsid-bridge
done

# ── Create universal binaries if needed ──────────────────────────────────────
if [[ "$ARCH" == "universal" ]]; then
    echo ""
    echo "==> Creating universal binaries ..."
    UNIVERSAL_DIR="$BUILD_DIR/universal"
    mkdir -p "$UNIVERSAL_DIR"

    lipo -create \
        "target/aarch64-apple-darwin/release/phosphor" \
        "target/x86_64-apple-darwin/release/phosphor" \
        -output "$UNIVERSAL_DIR/phosphor"

    lipo -create \
        "target/aarch64-apple-darwin/release/usbsid-bridge" \
        "target/x86_64-apple-darwin/release/usbsid-bridge" \
        -output "$UNIVERSAL_DIR/usbsid-bridge"

    BINARY_DIR="$UNIVERSAL_DIR"
else
    BINARY_DIR="target/${RUST_TARGETS[0]}/release"
fi

# ── Assemble .app bundle ────────────────────────────────────────────────────
echo ""
echo "==> Assembling $APP_NAME.app ..."

mkdir -p "$BUNDLE_DIR/Contents/MacOS"
mkdir -p "$BUNDLE_DIR/Contents/Helpers"
mkdir -p "$BUNDLE_DIR/Contents/Frameworks"
mkdir -p "$BUNDLE_DIR/Contents/Resources"

# Copy binaries
cp "$BINARY_DIR/phosphor"       "$BUNDLE_DIR/Contents/MacOS/phosphor"
cp "$BINARY_DIR/usbsid-bridge"  "$BUNDLE_DIR/Contents/Helpers/usbsid-bridge"

# Copy Info.plist with version substitution
sed "s/0\.2\.0/$VERSION/g" macos/Info.plist > "$BUNDLE_DIR/Contents/Info.plist"

# Copy launchd plist into Resources (for the daemon installer to find)
cp macos/com.phosphor.usbsid-bridge.plist "$BUNDLE_DIR/Contents/Resources/"

# Copy the daemon installer script into Resources
cp macos/install-daemon.sh "$BUNDLE_DIR/Contents/Resources/install-daemon.sh"
chmod +x "$BUNDLE_DIR/Contents/Resources/install-daemon.sh"

# ── Embed dynamic libraries (libusb, etc.) ──────────────────────────────────
#
# Hardened runtime requires every loaded dylib to share the same team ID.
# Homebrew's libusb is unsigned → dyld rejects it at launch:
#
#   "mapping process and mapped file (non-platform) have different Team IDs"
#
# Fix: copy every non-system dylib into Contents/Frameworks/, rewrite load
# paths to @rpath/ via install_name_tool, and sign them with the same
# identity as the binary.  This also means users don't need Homebrew.
# ─────────────────────────────────────────────────────────────────────────────
echo ""
echo "==> Embedding dynamic libraries ..."

FRAMEWORKS_DIR="$BUNDLE_DIR/Contents/Frameworks"

# Track dylibs we've already processed (file-based for subshell safety)
SEEN_FILE=$(mktemp)
trap "rm -f $SEEN_FILE" EXIT

collect_dylibs() {
    local binary="$1"
    # otool -L lists LC_LOAD_DYLIB entries; first line is the binary itself
    otool -L "$binary" 2>/dev/null | tail -n +2 | while read -r line; do
        # Extract path (before the "(compatibility version" part)
        local dylib
        dylib=$(echo "$line" | sed 's/^[[:space:]]*//' | sed 's/ (compatibility.*//')

        # Skip system libraries — always available, properly signed by Apple
        case "$dylib" in
            /usr/lib/*|/System/*) continue ;;
            @rpath/*|@executable_path/*|@loader_path/*) continue ;;
        esac

        # Skip duplicates
        if grep -qxF "$dylib" "$SEEN_FILE" 2>/dev/null; then
            continue
        fi
        echo "$dylib" >> "$SEEN_FILE"

        # Resolve symlinks
        local real_path
        real_path=$(realpath "$dylib" 2>/dev/null || echo "$dylib")

        if [[ ! -f "$real_path" ]]; then
            echo "    ⚠ dylib not found: $dylib (from $(basename "$binary"))"
            continue
        fi

        local libname
        libname=$(basename "$dylib")
        echo "    ✓ $libname  ← $real_path"

        # Copy into Frameworks/
        cp -L "$real_path" "$FRAMEWORKS_DIR/$libname"
        chmod 644 "$FRAMEWORKS_DIR/$libname"

        # Set the dylib's own install name to @rpath/<name>
        install_name_tool -id "@rpath/$libname" \
            "$FRAMEWORKS_DIR/$libname" 2>/dev/null || true

        # Recurse — this dylib might depend on other non-system dylibs
        collect_dylibs "$FRAMEWORKS_DIR/$libname"
    done
}

collect_dylibs "$BUNDLE_DIR/Contents/MacOS/phosphor"
collect_dylibs "$BUNDLE_DIR/Contents/Helpers/usbsid-bridge"

# ── Rewrite load paths to @rpath/ ───────────────────────────────────────────
echo ""
echo "==> Rewriting dylib load paths to @rpath ..."

rewrite_paths() {
    local target="$1"
    otool -L "$target" 2>/dev/null | tail -n +2 | while read -r line; do
        local dylib
        dylib=$(echo "$line" | sed 's/^[[:space:]]*//' | sed 's/ (compatibility.*//')

        case "$dylib" in
            /usr/lib/*|/System/*) continue ;;
            @rpath/*|@executable_path/*|@loader_path/*) continue ;;
        esac

        local libname
        libname=$(basename "$dylib")
        if [[ -f "$FRAMEWORKS_DIR/$libname" ]]; then
            install_name_tool -change "$dylib" "@rpath/$libname" "$target"
            echo "    $(basename "$target"): $dylib → @rpath/$libname"
        fi
    done
}

rewrite_paths "$BUNDLE_DIR/Contents/MacOS/phosphor"
rewrite_paths "$BUNDLE_DIR/Contents/Helpers/usbsid-bridge"

# Also rewrite inter-library references inside embedded dylibs
for fw in "$FRAMEWORKS_DIR"/*.dylib; do
    [[ -f "$fw" ]] && rewrite_paths "$fw"
done

# Add @rpath to both binaries → Contents/Frameworks/
#   MacOS/phosphor          →  @executable_path/../Frameworks  =  Contents/Frameworks ✓
#   Helpers/usbsid-bridge   →  @executable_path/../Frameworks  =  Contents/Frameworks ✓
echo ""
echo "==> Setting @rpath on binaries ..."
install_name_tool -add_rpath "@executable_path/../Frameworks" \
    "$BUNDLE_DIR/Contents/MacOS/phosphor" 2>/dev/null || true
install_name_tool -add_rpath "@executable_path/../Frameworks" \
    "$BUNDLE_DIR/Contents/Helpers/usbsid-bridge" 2>/dev/null || true

# ── Verify no external references remain ─────────────────────────────────────
echo ""
echo "==> Verifying no external dylib references remain ..."
EMBED_OK=true
for bin in "$BUNDLE_DIR/Contents/MacOS/phosphor" \
           "$BUNDLE_DIR/Contents/Helpers/usbsid-bridge" \
           "$FRAMEWORKS_DIR"/*.dylib; do
    [[ -f "$bin" ]] || continue
    BAD=$(otool -L "$bin" 2>/dev/null | tail -n +2 \
        | grep -v '/usr/lib/' \
        | grep -v '/System/' \
        | grep -v '@rpath/' \
        | grep -v '@executable_path/' \
        | grep -v '@loader_path/' || true)
    if [[ -n "$BAD" ]]; then
        echo "    ⚠ $(basename "$bin") still has external references:"
        echo "$BAD" | sed 's/^/        /'
        EMBED_OK=false
    else
        echo "    ✓ $(basename "$bin")"
    fi
done
if ! $EMBED_OK; then
    echo ""
    echo "    WARNING: Some external dylib references remain."
    echo "    The bundle may fail to load with hardened runtime."
fi

# ── Icon (PNG → icns) ───────────────────────────────────────────────────────
echo ""
echo "==> Generating app icon ..."
ICONSET_DIR="$BUILD_DIR/phosphor.iconset"
mkdir -p "$ICONSET_DIR"

if command -v sips &>/dev/null && command -v iconutil &>/dev/null; then
    for SIZE in 16 32 64 128 256 512; do
        sips -z $SIZE $SIZE assets/phosphor.png \
            --out "$ICONSET_DIR/icon_${SIZE}x${SIZE}.png" &>/dev/null
        DOUBLE=$((SIZE * 2))
        sips -z $DOUBLE $DOUBLE assets/phosphor.png \
            --out "$ICONSET_DIR/icon_${SIZE}x${SIZE}@2x.png" &>/dev/null
    done
    iconutil -c icns "$ICONSET_DIR" -o "$BUNDLE_DIR/Contents/Resources/phosphor.icns"
    echo "    ✓ phosphor.icns created"
else
    echo "    ⚠ sips/iconutil not available (not on macOS?), copying PNG as fallback"
    cp assets/phosphor.png "$BUNDLE_DIR/Contents/Resources/phosphor.png"
fi

# ── PkgInfo ──────────────────────────────────────────────────────────────────
echo -n "APPLPHOS" > "$BUNDLE_DIR/Contents/PkgInfo"

# ── Clean resource forks & metadata ──────────────────────────────────────────
# macOS cp/zip creates ._* and .DS_Store files that confuse codesign and pkgbuild
echo ""
echo "==> Cleaning resource forks and metadata ..."
find "$BUNDLE_DIR" -name '._*' -delete 2>/dev/null || true
find "$BUNDLE_DIR" -name '.DS_Store' -delete 2>/dev/null || true
# dot_clean merges ._* into extended attributes (safer than just deleting)
dot_clean "$BUNDLE_DIR" 2>/dev/null || true

# ── Code signing ─────────────────────────────────────────────────────────────
#
# Sign order: innermost → outermost
#   1. Frameworks/ (embedded dylibs)
#   2. Helpers/    (usbsid-bridge)
#   3. MacOS/      (phosphor)
#   4. Bundle envelope
#
# Everything gets the same identity so team IDs match at runtime.
# ─────────────────────────────────────────────────────────────────────────────
echo ""
echo "==> Signing bundle (identity: $SIGN_IDENTITY) ..."

# 1) Sign embedded frameworks
for fw in "$FRAMEWORKS_DIR"/*.dylib; do
    if [[ -f "$fw" ]]; then
        codesign --force --options runtime \
            --sign "$SIGN_IDENTITY" \
            "$fw"
        echo "    ✓ $(basename "$fw")"
    fi
done

# 2) Sign the bridge helper
codesign --force --options runtime \
    --entitlements macos/Bridge.entitlements \
    --sign "$SIGN_IDENTITY" \
    "$BUNDLE_DIR/Contents/Helpers/usbsid-bridge"
echo "    ✓ usbsid-bridge"

# 3) Sign the main executable
codesign --force --options runtime \
    --entitlements macos/Phosphor.entitlements \
    --sign "$SIGN_IDENTITY" \
    "$BUNDLE_DIR/Contents/MacOS/phosphor"
echo "    ✓ phosphor"

# 4) Sign the overall bundle
codesign --force --options runtime --deep \
    --entitlements macos/Phosphor.entitlements \
    --sign "$SIGN_IDENTITY" \
    "$BUNDLE_DIR"
echo "    ✓ $APP_NAME.app"

# Verify
echo ""
echo "==> Verifying signature ..."
codesign --verify --verbose=2 "$BUNDLE_DIR" 2>&1 | head -5 || true
codesign --verify --deep --strict "$BUNDLE_DIR" 2>&1 || true

# ── Create DMG ───────────────────────────────────────────────────────────────
if $CREATE_DMG; then
    echo ""
    echo "==> Creating DMG ..."
    DMG_NAME="Phosphor-${VERSION}-macOS-${ARCH}.dmg"
    DMG_PATH="$BUILD_DIR/$DMG_NAME"
    DMG_TEMP="$BUILD_DIR/dmg-stage"

    rm -rf "$DMG_TEMP" "$DMG_PATH"
    mkdir -p "$DMG_TEMP"
    cp -R "$BUNDLE_DIR" "$DMG_TEMP/"
    ln -s /Applications "$DMG_TEMP/Applications"

    hdiutil create -volname "$APP_NAME" \
        -srcfolder "$DMG_TEMP" \
        -ov -format UDZO \
        "$DMG_PATH"

    rm -rf "$DMG_TEMP"

    codesign --force --sign "$SIGN_IDENTITY" "$DMG_PATH"
    echo "    ✓ $DMG_NAME created"
fi

# ── Notarization ─────────────────────────────────────────────────────────────
if $NOTARIZE; then
    echo ""
    echo "==> Notarizing ..."

    TEAM_ID="${MACOS_TEAM_ID:?Set MACOS_TEAM_ID for notarization}"
    APPLE_ID="${MACOS_APPLE_ID:?Set MACOS_APPLE_ID for notarization}"
    APP_PASS="${MACOS_APP_PASSWORD:?Set MACOS_APP_PASSWORD for notarization}"

    if $CREATE_DMG; then
        NOTARIZE_FILE="$DMG_PATH"
    else
        NOTARIZE_FILE="$BUILD_DIR/Phosphor-${VERSION}.zip"
        ditto -c -k --keepParent "$BUNDLE_DIR" "$NOTARIZE_FILE"
    fi

    xcrun notarytool submit "$NOTARIZE_FILE" \
        --apple-id "$APPLE_ID" \
        --team-id "$TEAM_ID" \
        --password "$APP_PASS" \
        --wait

    if $CREATE_DMG; then
        xcrun stapler staple "$DMG_PATH"
    else
        xcrun stapler staple "$BUNDLE_DIR"
    fi

    echo "    ✓ Notarization complete"
fi

# ── Summary ──────────────────────────────────────────────────────────────────
echo ""
echo "╔══════════════════════════════════════════════════════════════╗"
echo "║  Build complete!"
echo "║"
echo "║  Bundle:  $BUNDLE_DIR"
if $CREATE_DMG; then
echo "║  DMG:     $DMG_PATH"
fi
echo "║"
echo "║  Embedded frameworks:"
ls -1 "$FRAMEWORKS_DIR" 2>/dev/null | sed 's/^/║    /' || echo "║    (none)"
echo "║"
echo "║  Install the USB bridge daemon:"
echo "║    $BUNDLE_DIR/Contents/Resources/install-daemon.sh"
echo "╚══════════════════════════════════════════════════════════════╝"
