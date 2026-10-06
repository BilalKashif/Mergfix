#!/bin/sh
# Build mergefix.app. With --install, also copy it to /Applications and point
# the `mergefix` command at the app's binary.
set -eu
cd "$(dirname "$0")/.."

cargo build --release
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
# Spotlight skips folders ending in .noindex, so only the installed copy shows up.
APP=target/bundle.noindex/mergefix.app
LSREGISTER=/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp target/release/mergefix "$APP/Contents/MacOS/mergefix"
sed "s/__VERSION__/$VERSION/g" macos/Info.plist > "$APP/Contents/Info.plist"

ICONSET=target/bundle.noindex/mergefix.iconset
rm -rf "$ICONSET" && mkdir -p "$ICONSET"
for s in 16 32 128 256 512; do
  sips -z $s $s assets/icon.png --out "$ICONSET/icon_${s}x${s}.png" >/dev/null
  sips -z $((s * 2)) $((s * 2)) assets/icon.png --out "$ICONSET/icon_${s}x${s}@2x.png" >/dev/null
done
iconutil -c icns "$ICONSET" -o "$APP/Contents/Resources/mergefix.icns"
rm -rf "$ICONSET"

codesign --force --sign - "$APP" >/dev/null 2>&1
echo "Built $APP"

if [ "${1:-}" = "--install" ]; then
  rm -rf /Applications/mergefix.app
  cp -R "$APP" /Applications/
  # Forget the build copy so Finder/"Open With" only offer the installed app.
  "$LSREGISTER" -u "$APP" 2>/dev/null || true
  "$LSREGISTER" -f /Applications/mergefix.app
  CARGO_BIN="${CARGO_HOME:-$HOME/.cargo}/bin"
  mkdir -p "$CARGO_BIN"
  ln -sf /Applications/mergefix.app/Contents/MacOS/mergefix "$CARGO_BIN/mergefix"
  echo "Installed /Applications/mergefix.app (the mergefix command now runs it too)"
fi
