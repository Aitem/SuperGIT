#!/bin/bash
# Build SuperGIT and install it for the current user (Linux, freedesktop):
#
#   scripts/install-linux.sh             → ~/.local/bin/supergit, a launcher in
#                                          ~/.local/share/applications, the icon
#   scripts/install-linux.sh --uninstall → removes those again
#
# PREFIX=/some/dir overrides ~/.local.
set -euo pipefail
cd "$(dirname "$0")/.."

prefix=${PREFIX:-$HOME/.local}
bin=$prefix/bin/supergit
desktop=$prefix/share/applications/supergit.desktop
icons=$prefix/share/icons/hicolor

refresh() {
  update-desktop-database -q "$prefix/share/applications" 2>/dev/null || true
  gtk-update-icon-cache -q -t "$icons" 2>/dev/null || true
}

if [[ "${1:-}" == "--uninstall" ]]; then
  rm -f "$bin" "$desktop" "$icons"/*/apps/supergit.png
  refresh
  echo "uninstalled SuperGIT from $prefix"
  exit 0
fi

cargo build --release --locked -p supergit

mkdir -p "$(dirname "$bin")" "$(dirname "$desktop")"
# Replace, don't overwrite in place: a running SuperGIT keeps its old binary.
install -m 755 target/release/supergit "$bin.new"
mv -f "$bin.new" "$bin"

for size in 128 256 512; do
  dir=$icons/${size}x${size}/apps
  mkdir -p "$dir"
  if command -v magick >/dev/null; then
    magick assets/icon-1024.png -resize "${size}x${size}" "$dir/supergit.png"
  elif (( size == 512 )); then
    cp assets/icon-1024.png "$dir/supergit.png" # desktops scale it down
  fi
done

# The notification code names this entry (`desktop-entry` hint), and the
# window's Wayland app id / X11 class is `supergit`.
cat > "$desktop" <<DESKTOP
[Desktop Entry]
Type=Application
Name=SuperGIT
GenericName=Git GUI
Comment=Git GUI for agent-driven development: worktrees, Claude Code and Codex sessions, commits, PRs
Exec=$bin %F
Icon=supergit
Terminal=false
Categories=Development;RevisionControl;
MimeType=inode/directory;
StartupWMClass=supergit
StartupNotify=true
DESKTOP
refresh

version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
echo "installed SuperGIT $version: $bin, $desktop"
case ":$PATH:" in *":$prefix/bin:"*) ;; *) echo "note: $prefix/bin is not on PATH" ;; esac
