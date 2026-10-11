#!/bin/sh
# Installs Abyss for the current user: binary, desktop entry, icon.
#
#   ./install.sh                         build (release) and install
#   ABYSS_ICON_SRC=art.svg ./install.sh  regenerate the icon from other art
#
# The icon (data/io.github.MatthewRafei.Abyss.svg) is octopus artwork from
# the Etsy shops VectorWhiz and CreativeSweetArt, used under a commercial
# licence; data/make-icon.py renders it from the original.
set -eu

here=$(cd "$(dirname "$0")" && pwd)
app_id=io.github.MatthewRafei.Abyss
bin_dir=$HOME/.local/bin
apps_dir=${XDG_DATA_HOME:-$HOME/.local/share}/applications
icon_dir=${XDG_DATA_HOME:-$HOME/.local/share}/icons/hicolor/scalable/apps

cd "$here"
cargo build --release
install -Dm755 target/release/abyss "$bin_dir/abyss"

# The app icon, and a symbolic one for the sidebar logo (GTK tints it).
mkdir -p "$icon_dir"
if [ -n "${ABYSS_ICON_SRC:-}" ]; then
    python3 data/make-icon.py "$ABYSS_ICON_SRC" "$icon_dir/$app_id.svg"
    python3 data/make-icon.py --symbolic "$ABYSS_ICON_SRC" "$icon_dir/$app_id-symbolic.svg"
else
    install -m644 "data/$app_id.svg" "$icon_dir/$app_id.svg"
    install -m644 "data/$app_id-symbolic.svg" "$icon_dir/$app_id-symbolic.svg"
fi

# Absolute Exec: launchers started by niri don't have ~/.local/bin in PATH.
mkdir -p "$apps_dir"
sed -e "s|^Exec=abyss|Exec=$bin_dir/abyss|" -e "s|^Icon=.*|Icon=$app_id|" \
    "data/$app_id.desktop" > "$apps_dir/$app_id.desktop"
if command -v update-desktop-database >/dev/null; then
    update-desktop-database "$apps_dir" 2>/dev/null || true
fi
echo "installed: $bin_dir/abyss, $apps_dir/$app_id.desktop"
