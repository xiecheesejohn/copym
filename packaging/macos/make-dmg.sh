#!/usr/bin/env bash
# Build copym.app and a drag-to-install DMG (macOS only).
#   packaging/macos/make-dmg.sh <copym binary> <version> <output.dmg>
set -euo pipefail
BIN=$1
VERSION=$2
OUT=$3
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../.." && pwd)
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

APP="$WORK/dmg/copym.app"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$BIN" "$APP/Contents/MacOS/copym"
chmod 755 "$APP/Contents/MacOS/copym"
sed "s/@VERSION@/$VERSION/g" "$HERE/Info.plist" > "$APP/Contents/Info.plist"
plutil -lint "$APP/Contents/Info.plist"

# Icon: 1024px PNG -> .icns
ICONSET="$WORK/copym.iconset"
mkdir "$ICONSET"
for s in 16 32 128 256 512; do
  sips -z $s $s "$ROOT/assets/icon.png" --out "$ICONSET/icon_${s}x${s}.png" >/dev/null
  d=$((s * 2))
  sips -z $d $d "$ROOT/assets/icon.png" --out "$ICONSET/icon_${s}x${s}@2x.png" >/dev/null
done
iconutil -c icns "$ICONSET" -o "$APP/Contents/Resources/copym.icns"

# Signature. With MACOS_SIGN_IDENTITY (a keychain identity, e.g. the SHA-1
# of the self-signed "copym Code Signing" certificate that CI imports) the
# designated requirement is "this bundle id + this certificate", so the
# Accessibility permission survives updates. Without it: an ad-hoc
# signature, which macOS ties to this exact build (re-allow after updates).
codesign --force --deep --sign "${MACOS_SIGN_IDENTITY:--}" --identifier io.github.xiecheesejohn.copym "$APP"
codesign --verify --deep --strict "$APP"
codesign --display --requirements - "$APP" 2>&1 | grep designated || true

ln -s /Applications "$WORK/dmg/Applications"
rm -f "$OUT"
hdiutil create -volname "copym $VERSION" -srcfolder "$WORK/dmg" -fs HFS+ -format UDZO -ov "$OUT" >/dev/null
echo "created $OUT"
