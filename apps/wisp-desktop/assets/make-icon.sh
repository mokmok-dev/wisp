#!/usr/bin/env bash
# Regenerate AppIcon.icns from icon-source.jpg.
#
# Runs the Swift renderer to produce a 1024×1024 master PNG, then derives
# every size macOS expects in an iconset and packages them with iconutil.
#
# Requires macOS (uses sips + iconutil + Swift system frameworks).

set -euo pipefail

cd "$(dirname "$0")"

SRC="icon-source.jpg"
ICNS="AppIcon.icns"

if [ ! -f "$SRC" ]; then
    echo "missing $SRC next to this script" >&2
    exit 1
fi

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

MASTER="$WORK/icon_1024.png"
ISET="$WORK/AppIcon.iconset"

echo "[1/3] rendering 1024×1024 master via Swift"
swift make-icon.swift "$SRC" "$MASTER"

echo "[2/3] generating iconset sizes"
mkdir "$ISET"
for sz in 16 32 64 128 256 512 1024; do
    sips -z "$sz" "$sz" "$MASTER" --out "$ISET/raw_$sz.png" > /dev/null
done
cp "$ISET/raw_16.png"   "$ISET/icon_16x16.png"
cp "$ISET/raw_32.png"   "$ISET/icon_16x16@2x.png"
cp "$ISET/raw_32.png"   "$ISET/icon_32x32.png"
cp "$ISET/raw_64.png"   "$ISET/icon_32x32@2x.png"
cp "$ISET/raw_128.png"  "$ISET/icon_128x128.png"
cp "$ISET/raw_256.png"  "$ISET/icon_128x128@2x.png"
cp "$ISET/raw_256.png"  "$ISET/icon_256x256.png"
cp "$ISET/raw_512.png"  "$ISET/icon_256x256@2x.png"
cp "$ISET/raw_512.png"  "$ISET/icon_512x512.png"
cp "$ISET/raw_1024.png" "$ISET/icon_512x512@2x.png"
rm "$ISET"/raw_*.png

echo "[3/3] packaging $ICNS"
iconutil -c icns "$ISET" -o "$ICNS"
echo "wrote $(pwd)/$ICNS"
