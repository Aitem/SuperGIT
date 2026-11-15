#!/bin/bash
# Regenerates assets/AppIcon.icns from scripts/make-icon.swift.
set -euo pipefail
cd "$(dirname "$0")/.."
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
swift scripts/make-icon.swift "$work/icon.png"
set="$work/AppIcon.iconset"
mkdir "$set"
for s in 16 32 128 256 512; do
  sips -z $s $s "$work/icon.png" --out "$set/icon_${s}x${s}.png" >/dev/null
  d=$((s * 2))
  sips -z $d $d "$work/icon.png" --out "$set/icon_${s}x${s}@2x.png" >/dev/null
done
iconutil -c icns "$set" -o assets/AppIcon.icns
cp "$work/icon.png" assets/icon-1024.png
echo "wrote assets/AppIcon.icns"
