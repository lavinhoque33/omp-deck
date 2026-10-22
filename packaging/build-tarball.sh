#!/bin/sh
# Builds dist/omp-deck-<version>-x86_64-linux.tar.gz from target/release/omp-deck.
# Used by CI (release.yml) and locally:  cargo build --release -p omp-deck && packaging/build-tarball.sh
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"

version=${1:-$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n1)}
bin=target/release/omp-deck
[ -x "$bin" ] || { echo "build-tarball: $bin missing; run cargo build --release -p omp-deck" >&2; exit 1; }

name="omp-deck-$version-x86_64-linux"
stage="dist/$name"
rm -rf "$stage"
mkdir -p "$stage/icons"
install -m 0755 "$bin" "$stage/omp-deck"
strip "$stage/omp-deck" 2>/dev/null || true
install -m 0644 packaging/omp-deck.desktop "$stage/omp-deck.desktop"
install -m 0644 packaging/icons/omp-deck.svg "$stage/icons/omp-deck.svg"
for png in packaging/icons/omp-deck-*.png; do
    install -m 0644 "$png" "$stage/icons/"
done
install -m 0755 packaging/install.sh "$stage/install.sh"
install -m 0644 LICENSE-MIT LICENSE-APACHE README.md "$stage/"
# Bundled third-party notices sit next to the project licenses.
mkdir -p "$stage/licenses"
install -m 0644 crates/omp-deck/assets/fonts/Geist-OFL.txt \
    crates/omp-deck/assets/fonts/lucide-LICENSE.txt "$stage/licenses/"

tar -C dist --owner=0 --group=0 --sort=name -czf "dist/$name.tar.gz" "$name"
echo "dist/$name.tar.gz"
