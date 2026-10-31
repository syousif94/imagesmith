#!/usr/bin/env bash
# Builds ImageSmith.app (and optionally a .dmg).
#
#   bundle/make-app.sh                 # release build → target/release/ImageSmith.app
#   bundle/make-app.sh --dmg           # also target/release/ImageSmith-<version>.dmg
#   SIGN_IDENTITY="Developer ID Application: …" bundle/make-app.sh
#
# Without SIGN_IDENTITY the bundle is signed ad hoc, which is enough to run it
# on this Mac.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT"
DMG=0
for a in "$@"; do
    case $a in
        --dmg) DMG=1 ;;
        *) echo "unknown option: $a" >&2; exit 2 ;;
    esac
done

TARGET_DIR=${CARGO_TARGET_DIR:-$ROOT/target}
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
BUILD=$(date +%Y%m%d%H%M)
APP=$TARGET_DIR/release/ImageSmith.app

echo "==> cargo build --release (app + CLI)"
cargo build --release -p imagesmith-app -p imagesmith-cli

echo "==> assembling $APP"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Helpers" "$APP/Contents/Resources/builder/fsctl"
cp "$TARGET_DIR/release/imagesmith-app" "$APP/Contents/MacOS/ImageSmith"
# Helpers/, not MacOS/: "imagesmith" and "ImageSmith" are the same name on APFS.
cp "$TARGET_DIR/release/imagesmith" "$APP/Contents/Helpers/imagesmith"
sed -e "s/@VERSION@/$VERSION/" -e "s/@BUILD@/$BUILD/" bundle/Info.plist > "$APP/Contents/Info.plist"
printf 'APPL????' > "$APP/Contents/PkgInfo"

# Builder sources: runner::builder_dir() looks in Contents/Resources/builder.
cp builder/Containerfile builder/build.sh "$APP/Contents/Resources/builder/"
cp -R builder/lib "$APP/Contents/Resources/builder/lib"
cp builder/fsctl/Cargo.toml "$APP/Contents/Resources/builder/fsctl/"
cp -R builder/fsctl/src "$APP/Contents/Resources/builder/fsctl/src"

echo "==> icon"
ICON_TMP=$(mktemp -d)
trap 'rm -rf "$ICON_TMP"' EXIT
swift bundle/make-icon.swift "$ICON_TMP/icon.png"
ICONSET=$ICON_TMP/AppIcon.iconset
mkdir -p "$ICONSET"
for s in 16 32 128 256 512; do
    sips -z $s $s "$ICON_TMP/icon.png" --out "$ICONSET/icon_${s}x${s}.png" >/dev/null
    sips -z $((s * 2)) $((s * 2)) "$ICON_TMP/icon.png" --out "$ICONSET/icon_${s}x${s}@2x.png" >/dev/null
done
iconutil -c icns "$ICONSET" -o "$APP/Contents/Resources/AppIcon.icns"

echo "==> codesign (${SIGN_IDENTITY:-ad hoc})"
codesign --force --sign "${SIGN_IDENTITY:--}" --options runtime --timestamp=none "$APP/Contents/Helpers/imagesmith"
codesign --force --sign "${SIGN_IDENTITY:--}" --options runtime --timestamp=none "$APP"
codesign --verify --strict "$APP"

if ((DMG)); then
    OUT=$TARGET_DIR/release/ImageSmith-$VERSION.dmg
    echo "==> $OUT"
    STAGE=$(mktemp -d)
    cp -R "$APP" "$STAGE/"
    ln -s /Applications "$STAGE/Applications"
    rm -f "$OUT"
    hdiutil create -volname ImageSmith -srcfolder "$STAGE" -ov -format UDZO "$OUT" >/dev/null
    rm -rf "$STAGE"
fi

echo "done: $APP"
