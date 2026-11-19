#!/bin/bash
# Download the Ghostty sources libghostty-vt is built from (the embedded
# terminal, crates/sg_terminal) into vendor/ghostty.
#
#   scripts/fetch-ghostty.sh
#
# `.cargo/config.toml` points GHOSTTY_SOURCE_DIR there, so `libghostty-vt-sys`
# builds with Zig from this tree instead of `git clone`-ing Ghostty (whole
# history) into every target directory during `cargo build`. Run it once per
# checkout, and again after bumping `libghostty-vt`: the commit below must be
# the one that crate's build.rs pins (GHOSTTY_COMMIT), or its bindings and
# the C library disagree.
#
# Needs curl and tar; building then needs Zig 0.17.x on PATH.
set -euo pipefail
cd "$(dirname "$0")/.."

commit=108cacb17ed8990840cd14abfce43d8ed8503ca1
dest=vendor/ghostty

if [[ -f "$dest/.ghostty-commit" && "$(cat "$dest/.ghostty-commit")" == "$commit" ]]; then
  echo "vendor/ghostty is already at $commit"
  exit 0
fi

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
echo "Downloading Ghostty $commit …"
curl -fsSL --retry 3 -o "$tmp/ghostty.tar.gz" \
  "https://codeload.github.com/ghostty-org/ghostty/tar.gz/$commit"
mkdir -p "$tmp/src"
tar -xzf "$tmp/ghostty.tar.gz" -C "$tmp/src" --strip-components 1
test -f "$tmp/src/build.zig"
echo "$commit" > "$tmp/src/.ghostty-commit"

rm -rf "$dest"
mkdir -p "$(dirname "$dest")"
mv "$tmp/src" "$dest"
echo "Ghostty sources in $dest"
