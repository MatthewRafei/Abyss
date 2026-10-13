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

# Other apps' file dialogs: D-Bus starts `abyss --portal` when one is
# wanted. A copy still serving from before exits a minute after its last
# dialog; the next one then runs the new binary.
dbus_dir=${XDG_DATA_HOME:-$HOME/.local/share}/dbus-1/services
portal_service=org.freedesktop.impl.portal.desktop.abyss.service
mkdir -p "$dbus_dir"
sed "s|^Exec=abyss|Exec=$bin_dir/abyss|" "data/$portal_service" > "$dbus_dir/$portal_service"
echo "installed: $bin_dir/abyss, $apps_dir/$app_id.desktop, $dbus_dir/$portal_service"

# xdg-desktop-portal only reads backends from its own data folder.
portal_file=/usr/share/xdg-desktop-portal/portals/abyss.portal
if ! cmp -s data/abyss.portal "$portal_file"; then
    cat <<EOF

To use Abyss for other apps' Open / Save dialogs, once (needs root):
  doas install -m644 $here/data/abyss.portal $portal_file
then add this under [preferred] in ~/.config/xdg-desktop-portal/portals.conf
  org.freedesktop.impl.portal.FileChooser=abyss;gtk
and restart the portal: pkill -x xdg-desktop-portal
EOF
fi
