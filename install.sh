#!/bin/sh
# Builds Television and installs it for the current user, under ~/.local (no root needed).
# Run it again to update; ./install.sh --uninstall removes it.
set -e
cd "$(dirname "$0")"
bin="$HOME/.local/bin/television"
icon="$HOME/.local/share/icons/hicolor/256x256/apps/television.png"
desktop="$HOME/.local/share/applications/television.desktop"
if [ "$1" = "--uninstall" ]; then
    rm -f "$bin" "$icon" "$desktop" "${XDG_CONFIG_HOME:-$HOME/.config}/autostart/television.desktop"
    echo "Removed. Settings are kept in ~/.config/com.teleqwe.television."
    exit
fi
cargo build --release
install -Dm755 target/release/television "$bin"
install -Dm644 icons/icon.png "$icon"
mkdir -p "$(dirname "$desktop")"
sed "s|^Exec=television|Exec=$bin|" packaging/television.desktop > "$desktop"
echo "Installed: Television is in your app launcher."
