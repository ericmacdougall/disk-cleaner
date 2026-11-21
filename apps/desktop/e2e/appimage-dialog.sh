#!/usr/bin/env bash
# The Linux AppImage on a host with no GTK (CT-F1, GoodQA #16452): the GTK folder dialog must
# open, return a folder and leave the app running on a distribution that has none of GTK,
# gdk-pixbuf, shared-mime-info or an icon theme. Everything the dialog needs must come from
# the AppImage itself (docs/DESKTOP.md, "Linux AppImage").
#
# Runs as root inside a bare container, the AppImage and this folder mounted read-only:
#   docker run --rm --cap-add SYS_PTRACE -v <AppImage>:/bundle/app.AppImage:ro \
#     -v apps/desktop/e2e:/e2e:ro -v <out>:/out fedora:41 bash /e2e/appimage-dialog.sh /bundle/app.AppImage /out
# Fedora and Debian (and derivatives) are supported. Steps:
#   1. installs only a display, a session and accessibility bus, AT-SPI's Python binding,
#      strace, and the libraries every desktop has that an AppImage leaves to the host (GL,
#      fontconfig, freetype, harfbuzz, fribidi, libgpg-error, one font);
#   2. fails (exit 2) unless the host is bare: no /usr/share/mime, no Adwaita or hicolor
#      index.theme, no gdk-pixbuf loader folder, no libgtk-3, no GTK file-chooser schema;
#   3. extracts the AppImage and starts its AppRun through e2e/idle-launch.sh (the write
#      observer, PR gate F10) under Xvfb and dbus-run-session, from a folder named
#      chosen-by-dialog;
#   4. the driver (this script with --driver) presses "Add folders..." through AT-SPI, waits
#      for the "Choose folders to scan" window, types this folder's path into it and presses Return, waits for the
#      folder to appear in the app's chosen list, checking the app is alive after each step;
#   5. fails if the app exited, the dialog did not open or return the folder, or GTK logged an
#      error, a critical, or a missing pixbuf, icon or icon theme.
# Two verdicts are printed, the dialog check (CT-F1) and the write observer (F10). Exit 0 when
# both pass, 1 when the dialog check fails, 3 when only the write observer fails, 2 on a host
# that is not bare or bad usage. The first argument may also be an extracted AppImage (AppDir).
set -euo pipefail

if [ "${1:-}" = --driver ]; then
  # Run by idle-launch.sh (IDLE_DRIVER) inside the display and bus; SANDBOX is set.
  HERE=$(cd "$(dirname "$0")" && pwd)
  V="$RESULT/dialog.txt"
  alive() { pgrep -x prunedisk-deskt > /dev/null; } # the process name, cut to 15 characters
  step() { echo "$*" | tee -a "$V"; }
  bad() { step "FAIL: $*"; exit 1; }
  alive || bad "the app is not running before the dialog"
  python3 "$HERE/atspi.py" press prunedisk "Add folders..." 60 || bad "no Add folders... button"
  opened=no
  for _ in $(seq 1 40); do
    if xdotool search --name '^Choose folders to scan$' > /dev/null 2>&1; then opened=yes; break; fi
    alive || break
    sleep 0.5
  done
  sleep 3 # the dialog draws its icons
  alive || bad "the app exited after Add folders... (dialog window seen: $opened)"
  [ "$opened" = yes ] || bad "the Choose folders to scan window did not appear"
  step "ok: the Choose folders to scan dialog opened and the app is alive"
  # The dialog opens on Recent, where Open is disabled and the location field only searches:
  # go to the home folder (Alt+Home), type this folder's path into the location field (Ctrl+L)
  # and press Return, as a user picking a folder by path does. Keys go to the focused window.
  win=$(xdotool search --name '^Choose folders to scan$' | head -1)
  xdotool windowfocus --sync "$win"
  xdotool key --clearmodifiers alt+Home
  sleep 1
  xdotool key --clearmodifiers ctrl+l
  sleep 0.5
  xdotool type --delay 20 "$PWD"
  sleep 0.5
  xdotool key --clearmodifiers Return
  python3 "$HERE/atspi.py" find prunedisk "Remove $PWD" 20 || bad "the chosen folder did not reach the app"
  sleep 2
  alive || bad "the app exited after the dialog returned"
  step "ok: the dialog returned the folder and the app is alive"
  exit 0
fi

APPIMAGE=${1:?AppImage}
RESULT=${2:?out folder}
HERE=$(cd "$(dirname "$0")" && pwd)
mkdir -p "$RESULT"
. /etc/os-release
echo "== host: $PRETTY_NAME"

case " $ID ${ID_LIKE:-} " in
  *" fedora "*)
    dnf -y -q install --setopt=install_weak_deps=False \
      xorg-x11-server-Xvfb xorg-x11-xauth xdotool dbus-daemon dbus-tools at-spi2-core \
      gsettings-desktop-schemas python3 python3-gobject-base strace procps-ng findutils util-linux \
      mesa-libGL mesa-libEGL libglvnd-gles mesa-dri-drivers fontconfig dejavu-sans-fonts fribidi libgpg-error \
      > "$RESULT/install.log" 2>&1 || { tail -30 "$RESULT/install.log"; exit 2; }
    rpm -qa | sort > "$RESULT/packages.txt"
    ;;
  *" debian "*)
    export DEBIAN_FRONTEND=noninteractive
    { apt-get update && apt-get install -y --no-install-recommends \
      xvfb xauth xdotool dbus dbus-bin at-spi2-core python3 python3-gi gir1.2-atspi-2.0 strace procps \
      libgl1 libegl1 libgles2 libgl1-mesa-dri libfontconfig1 libfreetype6 libharfbuzz0b fonts-dejavu-core \
      libfribidi0 libgpg-error0; } \
      > "$RESULT/install.log" 2>&1 || { tail -30 "$RESULT/install.log"; exit 2; }
    dpkg-query -W -f '${Package} ${Version}\n' | sort > "$RESULT/packages.txt"
    ;;
  *) echo "unsupported host $ID"; exit 2 ;;
esac
[ -s /etc/machine-id ] || dbus-uuidgen > /etc/machine-id

echo "== the host has none of what the dialog needs"
present=()
for p in /usr/share/mime /usr/share/icons/Adwaita/index.theme /usr/share/icons/hicolor/index.theme \
  /usr/share/glib-2.0/schemas/org.gtk.Settings.FileChooser.gschema.xml; do
  [ -e "$p" ] && present+=("$p")
done
while IFS= read -r p; do present+=("$p"); done < <(find /usr/lib /usr/lib64 /usr/lib32 -maxdepth 3 \
  \( -name gdk-pixbuf-2.0 -o -name 'libgtk-3.so*' -o -name 'libgdk_pixbuf-2.0.so*' \) 2> /dev/null || true)
if [ "${#present[@]}" -gt 0 ]; then
  printf '::error::not a bare host, it has %s\n' "${present[@]}"; exit 2
fi
echo "none present"

id -u tester > /dev/null 2>&1 || useradd -m tester
# A desktop session has a private runtime folder; dconf and the accessibility bus use it.
RUN_DIR=/run/user/$(id -u tester)
mkdir -p "$RUN_DIR"; chown tester "$RUN_DIR"; chmod 0700 "$RUN_DIR"
W=/tmp/appimage-dialog
rm -rf "$W"; mkdir -p "$W/chosen-by-dialog"
chown -R tester "$W" "$RESULT"
if [ -d "$APPIMAGE" ]; then
  # An already extracted AppImage (the AppDir): the planted control in desktop.yml passes one.
  cp -a "$APPIMAGE" "$W/squashfs-root"; chown -R tester "$W/squashfs-root"
  extract=:
else
  extract="'$APPIMAGE' --appimage-extract > /dev/null"
fi
cat > "$W/session.sh" << EOF
set -euo pipefail
/usr/libexec/at-spi-bus-launcher --launch-immediately > "$RESULT/atspi.log" 2>&1 &
sleep 1
cd "$W"
$extract
cd "$W/chosen-by-dialog"
export RESULT="$RESULT" WEBKIT_DISABLE_COMPOSITING_MODE=1
export IDLE_DRIVER="bash '$HERE/appimage-dialog.sh' --driver"
# Accepting the dialog adds the folder to GTK's recently used list (XDG_DATA_HOME/recently-used.xbel,
# written through a temporary file in the same folder): GTK's own record of a pick, not the user's data.
export IDLE_DECLARE="$RESULT/launch/sandbox/data/recently-used.xbel"
bash "$HERE/idle-launch.sh" "$W/squashfs-root/AppRun" "$RESULT/launch"
EOF
observer=0
su tester -s /bin/bash -c "export XDG_RUNTIME_DIR=$RUN_DIR; xvfb-run -a -s '-screen 0 1440x900x24' dbus-run-session -- bash '$W/session.sh'" \
  > "$RESULT/session.log" 2>&1 || observer=$?
cat "$RESULT/session.log"

echo "== GTK messages of the app"
log="$RESULT/launch/launch.log"
# GTK, GDK and gdk-pixbuf errors and criticals, and every sign of missing data. Atk and GObject
# criticals when the dialog closes come from the accessibility bridge this check turns on to
# press buttons; they are printed but do not fail the check.
pattern='Gtk-ERROR|Gtk:ERROR|Bail out|Gtk-CRITICAL|Gdk-CRITICAL|GdkPixbuf-|Could not load a pixbuf|Could not find the icon|theme was not found|Unrecognized image file format|Failed to load'
gtk_bad=$(grep -E "$pattern" "$log" 2> /dev/null || true)
grep -E 'Gtk|GLib|Gdk|Atk|pixbuf|icon' "$log" 2> /dev/null | head -40 || true

dialog=fail
if grep -q '^ok: the dialog returned the folder' "$RESULT/dialog.txt" 2> /dev/null && [ -z "$gtk_bad" ]; then dialog=pass; fi
echo "== verdicts"
if [ "$dialog" = pass ]; then
  echo "dialog check (CT-F1): PASS"
else
  echo "::error::dialog check (CT-F1): FAIL"
  cat "$RESULT/dialog.txt" 2> /dev/null || echo "(the driver recorded nothing)"
  [ -z "$gtk_bad" ] || printf 'GTK logged:\n%s\n' "$gtk_bad"
fi
if [ "$observer" = 0 ]; then echo "write observer (F10): PASS"; else echo "::error::write observer (F10): FAIL (idle-launch.sh exit $observer)"; fi
[ "$dialog" = pass ] || exit 1
[ "$observer" = 0 ] || exit 3
