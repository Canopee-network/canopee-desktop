#!/usr/bin/env bash
set -euo pipefail

# Builds dist/Canopee.app — a self-contained macOS menu bar app that is the
# only thing a user needs to install to run a full canopee node and handle
# canopee:// links.
#
# Usage:
#   ./scripts/build_app.sh                          # ad-hoc (unsigned) bundle
#   CODESIGN_IDENTITY="Developer ID Application: X" ./scripts/build_app.sh
#
# The app is signed but NOT notarized by default. To distribute outside your
# own machine, submit the bundle to Apple for notarization and staple it:
#   xcrun notarytool submit dist/Canopee.app --wait
#   xcrun stapler staple dist/Canopee.app

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
APP_NAME="Canopee"
BUNDLE_ID="com.canopee.tray"
IDENTITY="${CODESIGN_IDENTITY:-}"

DIST="$ROOT/dist"
APP="$DIST/$APP_NAME.app"

cargo build --release --manifest-path "$ROOT/Cargo.toml"
BIN="$ROOT/target/release/canopee-tray"

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"

cp "$BIN" "$APP/Contents/MacOS/canopee-tray"
chmod +x "$APP/Contents/MacOS/canopee-tray"

# App icon: upscaled from the menu glyph. Swap APP_ICON_SRC for a real
# high-resolution 1024x1024 artwork when one exists.
ICON_SRC="${APP_ICON_SRC:-$ROOT/src/icons/constellation_24_b.png}"
ICONSET="$DIST/Canopee.iconset"
rm -rf "$ICONSET" "$APP/Contents/Resources/Canopee.icns"
mkdir -p "$ICONSET"
for size in 16 32 128 256 512; do
  sips -z "$size" "$size" "$ICON_SRC" --out "$ICONSET/icon_${size}x${size}.png" >/dev/null
  sips -z "$((size * 2))" "$((size * 2))" "$ICON_SRC" --out "$ICONSET/icon_${size}x${size}@2x.png" >/dev/null
done
iconutil -c icns "$ICONSET" -o "$APP/Contents/Resources/Canopee.icns"
rm -rf "$ICONSET"

cat > "$APP/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleName</key>
	<string>Canopee</string>
	<key>CFBundleDisplayName</key>
	<string>Canopee</string>
	<key>CFBundleIdentifier</key>
	<string>com.canopee.tray</string>
	<key>CFBundleVersion</key>
	<string>0.1.0</string>
	<key>CFBundleShortVersionString</key>
	<string>0.1.0</string>
	<key>CFBundleExecutable</key>
	<string>canopee-tray</string>
	<key>CFBundlePackageType</key>
	<string>APPL</string>
	<key>CFBundleIconFile</key>
	<string>Canopee</string>
	<key>CFBundleInfoDictionaryVersion</key>
	<string>6.0</string>
	<key>NSHighResolutionCapable</key>
	<true/>
	<key>LSMinimumSystemVersion</key>
	<string>11.0</string>
	<key>LSUIElement</key>
	<true/>
	<key>NSHumanReadableCopyright</key>
	<string>Canopee</string>
	<key>CFBundleURLTypes</key>
	<array>
		<dict>
			<key>CFBundleURLName</key>
			<string>Canopee Identity Links</string>
			<key>CFBundleURLSchemes</key>
			<array>
				<string>canopee</string>
			</array>
		</dict>
	</array>
</dict>
</plist>
PLIST

if [[ -z "$IDENTITY" ]]; then
  codesign --force --sign - "$APP" >/dev/null
else
  codesign --force --deep --sign "$IDENTITY" "$APP"
fi

# Register the bundle with LaunchServices so `open canopee://...` routes here.
LSREGISTER="/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister"
"$LSREGISTER" -f "$APP" >/dev/null

echo "Built $APP"
echo "Launch with: open \"$APP\""
echo "Test a canopee:// link with: open 'canopee://identity/<peer-id>/<app>'"