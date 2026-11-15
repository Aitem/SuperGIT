#!/bin/bash
# Build SuperGIT.app from the current checkout.
#
#   scripts/bundle-macos.sh             → target/release/bundle.noindex/SuperGIT.app
#   scripts/bundle-macos.sh --install   → moves it to /Applications (one copy only)
#
# The staging folder ends in `.noindex` so Spotlight/Launchpad never list the
# build copy as a second SuperGIT.
#
# Local builds only: the bundle is ad-hoc signed (no Developer ID/notarization),
# which macOS accepts for apps built on this machine.
set -euo pipefail
cd "$(dirname "$0")/.."

install=0
[[ "${1:-}" == "--install" ]] && install=1

cargo build --release -p supergit

version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
build=$(git rev-list --count HEAD 2>/dev/null || echo 1)
sha=$(git rev-parse --short HEAD 2>/dev/null || echo dev)

lsregister=/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister
# Older scripts staged into an indexed folder: forget that copy.
if [[ -d target/release/bundle/SuperGIT.app ]]; then
  "$lsregister" -u target/release/bundle/SuperGIT.app 2>/dev/null || true
  rm -rf target/release/bundle
fi

app=target/release/bundle.noindex/SuperGIT.app
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp target/release/supergit "$app/Contents/MacOS/supergit"
cp assets/AppIcon.icns "$app/Contents/Resources/AppIcon.icns"
sed -e "s/@VERSION@/$version/" -e "s/@BUILD@/$build.$sha/" assets/Info.plist > "$app/Contents/Info.plist"
plutil -lint "$app/Contents/Info.plist" >/dev/null
codesign --force --sign - --timestamp=none "$app" 2>/dev/null
echo "built $app ($version, build $build.$sha)"

if (( install )); then
  dest=/Applications/SuperGIT.app
  if pgrep -qf "$dest/Contents/MacOS/supergit"; then
    echo "SuperGIT is running — quitting it to replace the bundle"
    osascript -e 'tell application "SuperGIT" to quit' >/dev/null 2>&1 || true
    for _ in 1 2 3 4 5 6 7 8 9 10; do pgrep -qf "$dest/Contents/MacOS/supergit" || break; sleep 0.5; done
  fi
  rm -rf "$dest"
  mv "$app" "$dest"
  # Refresh Launchpad/Spotlight/Dock metadata (icon, document types).
  "$lsregister" -f "$dest" || true
  touch "$dest"
  echo "installed $dest"
fi
