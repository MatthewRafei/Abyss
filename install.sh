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
dbus_dir=${XDG_DATA_HOME:-$HOME/.local/share}/dbus-1/services
portal_dir=${XDG_DATA_HOME:-$HOME/.local/share}/xdg-desktop-portal/portals

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

# FileManager1 (other apps' "Show in Folder"): the user's service file
# takes precedence over a system file manager's.
mkdir -p "$dbus_dir"
cat > "$dbus_dir/org.freedesktop.FileManager1.service" <<EOF
[D-BUS Service]
Name=org.freedesktop.FileManager1
Exec=$bin_dir/abyss --gapplication-service
EOF

# FileChooser portal (other apps' Open and Save dialogs). xdg-desktop-portal
# reads backends from one folder, /usr/share/xdg-desktop-portal/portals,
# unless XDG_DESKTOP_PORTAL_DIR names another: this user folder, holding
# Abyss's backend and links to the system's. With it set, the portal also
# reads its *-portals.conf only from there, so those are linked in too
# (the user's over the system's). Re-run after installing another portal
# backend or adding a portals.conf.
# Started by the portal, Abyss must not ask the portal anything while GTK
# starts (it waits for Abyss meanwhile): no-portals, as portal-gtk does.
cat > "$dbus_dir/org.freedesktop.impl.portal.desktop.abyss.service" <<EOF
[D-BUS Service]
Name=org.freedesktop.impl.portal.desktop.abyss
Exec=/usr/bin/env GDK_DEBUG=no-portals $bin_dir/abyss --gapplication-service
EOF
mkdir -p "$portal_dir"
for p in /usr/share/xdg-desktop-portal/portals/*.portal \
    /usr/share/xdg-desktop-portal/*portals.conf \
    /etc/xdg/xdg-desktop-portal/*portals.conf \
    "${XDG_CONFIG_HOME:-$HOME/.config}"/xdg-desktop-portal/*portals.conf; do
    [ -e "$p" ] && ln -sf "$p" "$portal_dir/"
done
install -m644 data/abyss.portal "$portal_dir/abyss.portal"
# systemd starts the portal as a user service: point it at the folder.
dropin=${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user/xdg-desktop-portal.service.d
if systemctl --user show-environment >/dev/null 2>&1; then
    mkdir -p "$dropin"
    printf '[Service]\nEnvironment=XDG_DESKTOP_PORTAL_DIR=%s\n' "$portal_dir" > "$dropin/abyss.conf"
    systemctl --user daemon-reload
    echo "portal: restart it with: systemctl --user restart xdg-desktop-portal"
else
    echo "portal: start xdg-desktop-portal with XDG_DESKTOP_PORTAL_DIR=$portal_dir"
fi
echo "portal: choose Abyss in ~/.config/xdg-desktop-portal/<desktop>-portals.conf:"
echo "    org.freedesktop.impl.portal.FileChooser=abyss;gtk;"
echo "installed: $bin_dir/abyss, $apps_dir/$app_id.desktop, $dbus_dir/org.freedesktop.FileManager1.service, $portal_dir/abyss.portal"
