#!/bin/bash
# Build intendant as a native macOS desktop app.
#
# Creates Intendant.app with a Swift wrapper that:
#   1. Launches intendant --web as a child process
#   2. Opens a native window with WKWebView loading the dashboard
#   3. Gets proper TCC permissions (Screen Recording, Accessibility)
#   4. Child processes (ffmpeg, screencapture, cliclick) inherit permissions
#
# Usage:
#   ./scripts/bundle-macos.sh          # Release build
#   ./scripts/bundle-macos.sh debug    # Debug build
#
# Output: target/Intendant.app

set -euo pipefail

BUNDLE_ID="com.intendant.app"
RESET_PERMS=false

for arg in "$@"; do
    case "$arg" in
        --reset-permissions) RESET_PERMS=true ;;
    esac
done

# Filter out flags to get positional args
PROFILE="release"
for arg in "$@"; do
    case "$arg" in
        --*) ;;
        *) PROFILE="$arg" ;;
    esac
done

if [ "$PROFILE" = "debug" ]; then
    BINARY="target/debug/intendant"
    RUNTIME="target/debug/intendant-runtime"
    cargo build
else
    BINARY="target/release/intendant"
    RUNTIME="target/release/intendant-runtime"
    cargo build --release
fi

APP="target/Intendant.app"
CONTENTS="$APP/Contents"
MACOS="$CONTENTS/MacOS"
RESOURCES="$CONTENTS/Resources"

rm -rf "$APP"
mkdir -p "$MACOS" "$RESOURCES"

# Compile Swift wrapper
echo "Compiling macOS app wrapper..."
swiftc -O -o "$MACOS/Intendant" macos-app/main.swift \
    -framework Cocoa -framework WebKit

# Copy Rust binaries
cp "$BINARY" "$MACOS/intendant-bin"
cp "$RUNTIME" "$MACOS/intendant-runtime"

# Copy app icon
if [ -f "macos-app/AppIcon.icns" ]; then
    cp "macos-app/AppIcon.icns" "$RESOURCES/AppIcon.icns"
fi

# Info.plist
cat > "$CONTENTS/Info.plist" << 'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleExecutable</key>
    <string>Intendant</string>
    <key>CFBundleIdentifier</key>
    <string>com.intendant.app</string>
    <key>CFBundleName</key>
    <string>Intendant</string>
    <key>CFBundleDisplayName</key>
    <string>Intendant</string>
    <key>CFBundleVersion</key>
    <string>1.0</string>
    <key>CFBundleShortVersionString</key>
    <string>1.0</string>
    <key>CFBundleIconFile</key>
    <string>AppIcon</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>LSMinimumSystemVersion</key>
    <string>14.0</string>
    <key>NSHighResolutionCapable</key>
    <true/>
    <key>NSScreenCaptureUsageDescription</key>
    <string>Intendant records your screen for display capture, computer use, and session replay.</string>
    <key>NSAppleEventsUsageDescription</key>
    <string>Intendant uses AppleScript for keyboard/mouse automation and system control.</string>
    <key>NSMicrophoneUsageDescription</key>
    <string>Intendant uses the microphone for voice conversations with the AI presence layer.</string>
    <key>NSCameraUsageDescription</key>
    <string>Intendant uses the camera for video input to the AI presence layer.</string>
</dict>
</plist>
PLIST

if [ "$RESET_PERMS" = true ]; then
    echo "Resetting TCC permissions for $BUNDLE_ID..."
    tccutil reset ScreenCapture "$BUNDLE_ID" 2>/dev/null || true
    tccutil reset Accessibility "$BUNDLE_ID" 2>/dev/null || true
    tccutil reset Microphone "$BUNDLE_ID" 2>/dev/null || true
    tccutil reset Camera "$BUNDLE_ID" 2>/dev/null || true
    echo "Permissions reset — macOS will re-prompt on next launch."
fi

echo "✅ Built: $APP"
echo ""
echo "Launch:"
echo "  open target/Intendant.app"
echo ""
echo "If permissions seem stuck, rebuild with --reset-permissions:"
echo "  ./scripts/bundle-macos.sh --reset-permissions"
