#!/bin/sh
# Installs Stream Tube for the current user (no root needed).
# Usage: ./install.sh            install
#        ./install.sh --uninstall remove
set -e
here="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd -P)"
bin="$HOME/.local/bin"
apps="${XDG_DATA_HOME:-$HOME/.local/share}/applications"
icons="${XDG_DATA_HOME:-$HOME/.local/share}/icons/hicolor/256x256/apps"

if [ "$1" = "--uninstall" ]; then
    rm -f "$bin/streamtube" "$apps/streamtube.desktop" "$icons/streamtube.png"
    echo "Stream Tube removed. Your data is still in ~/.local/share/StreamTube"
    exit 0
fi

mkdir -p "$bin" "$apps" "$icons"
install -m 755 "$here/streamtube" "$bin/streamtube"
install -m 644 "$here/streamtube.png" "$icons/streamtube.png"
sed "s|^Exec=streamtube|Exec=$bin/streamtube|" "$here/streamtube.desktop" > "$apps/streamtube.desktop"
command -v update-desktop-database >/dev/null 2>&1 && update-desktop-database "$apps" >/dev/null 2>&1 || true
# streamtube:// links (the browser userscript) open the app
command -v xdg-mime >/dev/null 2>&1 && xdg-mime default streamtube.desktop x-scheme-handler/streamtube >/dev/null 2>&1 || true

echo "Stream Tube installed to $bin/streamtube"
if ! ldconfig -p 2>/dev/null | grep -q "libmpv\.so"; then
    echo
    echo "Stream Tube plays video through libmpv, which isn't installed yet:"
    echo "  Debian/Ubuntu:  sudo apt install libmpv2   (Ubuntu 22.04: libmpv1)"
    echo "  Fedora:         sudo dnf install mpv-libs"
    echo "  Arch:           sudo pacman -S mpv"
fi
echo "Downloads above 360p use yt-dlp and FFmpeg. Install them with your package manager,"
echo "or let Stream Tube fetch them (Settings -> Downloads)."
