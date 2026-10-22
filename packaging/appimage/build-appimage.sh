#!/bin/sh
# Builds dist/omp-deck-<version>-x86_64.AppImage from target/release/omp-deck.
# Needs appimagetool on PATH (or APPIMAGETOOL=/path/to/appimagetool). In CI it is downloaded first.
# The AppImage bundles only OmpDeck itself: omp and (optionally) tmux come from the host.
set -eu

root=$(cd "$(dirname "$0")/../.." && pwd)
cd "$root"

version=${1:-$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n1)}
tool=${APPIMAGETOOL:-appimagetool}
bin=target/release/omp-deck
[ -x "$bin" ] || { echo "build-appimage: $bin missing; run cargo build --release -p omp-deck" >&2; exit 1; }

appdir=dist/OmpDeck.AppDir
rm -rf "$appdir"
mkdir -p "$appdir/usr/bin" "$appdir/usr/share/applications" "$appdir/usr/share/icons/hicolor/scalable/apps"
install -m 0755 "$bin" "$appdir/usr/bin/omp-deck"
strip "$appdir/usr/bin/omp-deck" 2>/dev/null || true
install -m 0755 packaging/appimage/AppRun "$appdir/AppRun"
install -m 0644 packaging/omp-deck.desktop "$appdir/omp-deck.desktop"
install -m 0644 packaging/omp-deck.desktop "$appdir/usr/share/applications/omp-deck.desktop"
install -m 0644 packaging/icons/omp-deck-256.png "$appdir/omp-deck.png"
cp "$appdir/omp-deck.png" "$appdir/.DirIcon"
install -m 0644 packaging/icons/omp-deck.svg "$appdir/usr/share/icons/hicolor/scalable/apps/omp-deck.svg"
for s in 16 24 32 48 64 128 256 512; do
    mkdir -p "$appdir/usr/share/icons/hicolor/${s}x${s}/apps"
    install -m 0644 "packaging/icons/omp-deck-$s.png" "$appdir/usr/share/icons/hicolor/${s}x${s}/apps/omp-deck.png"
done

out="dist/omp-deck-$version-x86_64.AppImage"
ARCH=x86_64 "$tool" --appimage-extract-and-run "$appdir" "$out" 2>/dev/null \
    || ARCH=x86_64 "$tool" "$appdir" "$out"
echo "$out"
