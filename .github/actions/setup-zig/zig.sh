#!/usr/bin/env bash
# The Zig release CI builds libghostty-vt with (crates/sg_terminal).
#
#   zig.sh pick     names this runner's package and where it goes ($GITHUB_OUTPUT)
#   zig.sh install  downloads it unless the cache brought it, puts it on PATH
#
# Bump ZIG_VERSION together with mise.toml; take the new SHA-256s from
# https://ziglang.org/download/index.json ("shasum" of each platform).
set -euo pipefail

ZIG_VERSION=0.17.1

case "$RUNNER_OS-$RUNNER_ARCH" in
  macOS-ARM64)
    platform=aarch64-macos
    sha256=75f1bc7a1e9852f17ec9a0c1895f767c4ec56d46bc8a834920eece1d0359dd8d
    ;;
  Linux-X64)
    platform=x86_64-linux
    sha256=3581863e3cbcfd0fce0efc682ce7e9e93da96668a977791d356a26d2a7e9b17a
    ;;
  Linux-ARM64)
    platform=aarch64-linux
    sha256=a1546d5646eb184437918d6635ca6b762a0e889eac09cef720ee7a1ed3fe7795
    ;;
  *)
    echo "::error::No Zig package listed for $RUNNER_OS $RUNNER_ARCH (.github/actions/setup-zig/zig.sh)"
    exit 1
    ;;
esac

name="zig-$platform-$ZIG_VERSION"
dir="$RUNNER_TOOL_CACHE/$name"

download() {
  local tarball="$name.tar.xz"
  local file="$RUNNER_TEMP/$tarball"
  # Zig asks automated downloads to use its community mirrors, picked at
  # random; ziglang.org comes last. The checksum makes any mirror as good as
  # the source, and only https URLs are taken from the list. Three mirrors at
  # most, and a stalled transfer moves on: a wrong checksum fails in minutes.
  local mirrors
  mirrors=$(curl -fsSL --retry 2 https://ziglang.org/download/community-mirrors.txt |
    awk 'BEGIN { srand() } /^https:\/\/[^ ]+$/ { print rand() "\t" $1 }' | sort -n | cut -f2- | head -n 3) || true
  local base
  for base in $mirrors "https://ziglang.org/download/$ZIG_VERSION"; do
    if curl -fsSL --connect-timeout 10 --speed-limit 100000 --speed-time 20 -o "$file" "$base/$tarball" &&
      echo "$sha256  $file" | shasum -a 256 -c -s -; then
      echo "Downloaded $tarball from $base"
      mkdir -p "$dir"
      tar -xJf "$file" -C "$dir" --strip-components=1
      rm -f "$file"
      return
    fi
    echo "No valid $tarball from $base"
  done
  echo "::error::Could not download $tarball from any mirror"
  exit 1
}

case "${1:-}" in
  pick)
    echo "name=$name" >>"$GITHUB_OUTPUT"
    echo "dir=$dir" >>"$GITHUB_OUTPUT"
    ;;
  install)
    [[ -x "$dir/zig" ]] || download
    echo "$dir" >>"$GITHUB_PATH"
    "$dir/zig" version
    ;;
  *)
    echo "usage: zig.sh pick|install" >&2
    exit 2
    ;;
esac
