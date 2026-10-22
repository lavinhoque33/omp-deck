#!/bin/sh
# OmpDeck installer (per-user, no root).
#
#   curl -fsSL https://raw.githubusercontent.com/lavinhoque33/omp-deck/main/packaging/install.sh | sh
#   ./install.sh              from an extracted release tarball: install that build
#   ./install.sh --uninstall  remove everything this script installed
#
# Options: --version <x.y.z>  install a specific release instead of the latest
#          --prefix <dir>     install under <dir> instead of ~/.local
#          --uninstall        remove the installed files
set -eu

REPO="lavinhoque33/omp-deck"
APP="omp-deck"
SIZES="16 24 32 48 64 128 256 512"

prefix="${HOME}/.local"
version=""
uninstall=0

say() { printf '%s\n' "$*"; }
warn() { printf 'warning: %s\n' "$*" >&2; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }

while [ $# -gt 0 ]; do
    case "$1" in
        --uninstall) uninstall=1 ;;
        --version) [ $# -ge 2 ] || die "--version needs a value"; version=${2#v}; shift ;;
        --prefix) [ $# -ge 2 ] || die "--prefix needs a value"; prefix=$2; shift ;;
        -h|--help) sed -n '2,10p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) die "unknown option: $1" ;;
    esac
    shift
done

data_home="${XDG_DATA_HOME:-}"
if [ "$prefix" != "${HOME}/.local" ] || [ -z "$data_home" ]; then
    data_home="$prefix/share"
fi
bin_dir="$prefix/bin"
apps_dir="$data_home/applications"
icon_root="$data_home/icons/hicolor"

refresh_caches() {
    if command -v update-desktop-database >/dev/null 2>&1; then
        update-desktop-database "$apps_dir" >/dev/null 2>&1 || true
    fi
    if command -v gtk-update-icon-cache >/dev/null 2>&1 && [ -f "$icon_root/index.theme" ]; then
        gtk-update-icon-cache -q -t -f "$icon_root" >/dev/null 2>&1 || true
    fi
}

if [ "$uninstall" -eq 1 ]; then
    rm -f "$bin_dir/$APP" "$apps_dir/$APP.desktop" "$icon_root/scalable/apps/$APP.svg"
    for s in $SIZES; do
        rm -f "$icon_root/${s}x${s}/apps/$APP.png"
    done
    # Remove the icon directories the install created, but only while they are empty
    # (rmdir refuses to touch a directory that holds another application's files).
    for s in $SIZES; do
        rmdir "$icon_root/${s}x${s}/apps" "$icon_root/${s}x${s}" 2>/dev/null || true
    done
    rmdir "$icon_root/scalable/apps" "$icon_root/scalable" "$icon_root" 2>/dev/null || true
    rmdir "$(dirname "$icon_root")" 2>/dev/null || true
    refresh_caches
    say "OmpDeck uninstalled from $prefix."
    say "Settings in \${XDG_CONFIG_HOME:-~/.config}/omp-deck and data in \${XDG_DATA_HOME:-~/.local/share}/omp-deck were left in place."
    exit 0
fi

[ "$(uname -s)" = "Linux" ] || die "OmpDeck only supports Linux"
case "$(uname -m)" in
    x86_64|amd64) ;;
    *) die "release builds exist for x86_64 only (this machine: $(uname -m)); build from source instead" ;;
esac

# Source directory: the extracted tarball this script sits in, else a fresh download.
src=""
tmp=""
cleanup() { [ -z "$tmp" ] || rm -rf "$tmp"; }
trap cleanup EXIT INT TERM

script_dir=""
case "$0" in
    */*) script_dir=$(cd "$(dirname "$0")" 2>/dev/null && pwd) || script_dir="" ;;
    # `sh install.sh` leaves $0 without a slash; the tarball payload is in the cwd then.
    *) if [ -f ./install.sh ]; then script_dir=$(pwd); fi ;;
esac
if [ -n "$script_dir" ] && [ -x "$script_dir/$APP" ] && [ -f "$script_dir/$APP.desktop" ] && [ -z "$version" ]; then
    src=$script_dir
else
    command -v tar >/dev/null 2>&1 || die "tar is required"
    if command -v curl >/dev/null 2>&1; then
        fetch() { curl -fsSL "$1"; }
        fetch_to() { curl -fsSL -o "$2" "$1"; }
    elif command -v wget >/dev/null 2>&1; then
        fetch() { wget -qO- "$1"; }
        fetch_to() { wget -qO "$2" "$1"; }
    else
        die "curl or wget is required"
    fi
    if [ -z "$version" ]; then
        say "Looking up the latest release..."
        version=$(fetch "https://api.github.com/repos/$REPO/releases/latest" \
            | sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"v\{0,1\}\([^"]*\)".*/\1/p' | head -n1) || true
        [ -n "$version" ] || die "could not determine the latest release (see https://github.com/$REPO/releases)"
    fi
    name="$APP-$version-x86_64-linux"
    url="https://github.com/$REPO/releases/download/v$version/$name.tar.gz"
    tmp=$(mktemp -d)
    say "Downloading $url"
    fetch_to "$url" "$tmp/$name.tar.gz" || die "download failed"
    if fetch_to "$url.sha256" "$tmp/$name.tar.gz.sha256" 2>/dev/null && command -v sha256sum >/dev/null 2>&1; then
        (cd "$tmp" && sha256sum -c "$name.tar.gz.sha256" >/dev/null) || die "checksum mismatch"
    else
        warn "checksum not verified"
    fi
    tar -xzf "$tmp/$name.tar.gz" -C "$tmp"
    src="$tmp/$name"
    [ -x "$src/$APP" ] || die "unexpected archive layout"
fi

mkdir -p "$bin_dir" "$apps_dir"
install -m 0755 "$src/$APP" "$bin_dir/$APP"

# The desktop entry needs an absolute Exec so launchers work even when ~/.local/bin is not on PATH.
sed -e "s|^Exec=.*|Exec=$bin_dir/$APP %f|" -e "s|^TryExec=.*|TryExec=$bin_dir/$APP|" \
    "$src/$APP.desktop" > "$apps_dir/$APP.desktop"
chmod 0644 "$apps_dir/$APP.desktop"

for s in $SIZES; do
    if [ -f "$src/icons/$APP-$s.png" ]; then
        mkdir -p "$icon_root/${s}x${s}/apps"
        install -m 0644 "$src/icons/$APP-$s.png" "$icon_root/${s}x${s}/apps/$APP.png"
    fi
done
if [ -f "$src/icons/$APP.svg" ]; then
    mkdir -p "$icon_root/scalable/apps"
    install -m 0644 "$src/icons/$APP.svg" "$icon_root/scalable/apps/$APP.svg"
fi
refresh_caches

say "Installed OmpDeck to $bin_dir/$APP"
command -v omp >/dev/null 2>&1 || warn "'omp' was not found on PATH; OmpDeck needs it (https://github.com/can1357/oh-my-pi)"
command -v tmux >/dev/null 2>&1 || warn "'tmux' was not found; OmpDeck still works but sessions will not persist after closing a tab. Install tmux for persistence."
case ":$PATH:" in
    *":$bin_dir:"*) ;;
    *) warn "$bin_dir is not on your PATH; add it to launch 'omp-deck' from a shell" ;;
esac
