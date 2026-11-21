#!/usr/bin/env bash
# What a screen reader gets from the size tree of the shipped Linux AppImage (F16; CT-F4, CT-F5,
# gate F6): e2e/a11y/treegrid_atspi.py run in a bare container, as in e2e/appimage-dialog.sh.
#
# Runs as root inside a container, the AppImage and apps/desktop/e2e mounted read-only:
#   docker run --rm -v <AppImage>:/bundle/app.AppImage:ro -v apps/desktop/e2e:/e2e:ro \
#     -v <out>:/out fedora:41 bash /e2e/a11y/treegrid-host.sh /bundle/app.AppImage /out
# Fedora and Debian (and derivatives). Steps:
#   1. installs a display, a session and accessibility bus, AT-SPI's Python bindings (gi and
#      pyatspi), xdotool and the libraries an AppImage leaves to the host;
#   2. makes /home/tester/tree/wide with 5,000 empty files (the folder the probe opens);
#   3. starts the extracted AppRun as the tester under Xvfb and dbus-run-session;
#   4. treegrid_setup.py ticks the / volume, presses Scan and opens the Size tree tab;
#   5. treegrid_atspi.py check drives the treegrid keys and judges what the reader is told,
#      including each focused object's AT-SPI role against its computed role.
# Exit 0 when the probe passes, 1 when it or the setup fails, 2 on bad usage or a failed install.
# Results in the out folder: setup.txt, treegrid.txt (the probe's report), app.log, session.log.
set -euo pipefail
APPIMAGE=${1:?AppImage}
RESULT=${2:?out folder}
HERE=$(cd "$(dirname "$0")" && pwd)
mkdir -p "$RESULT"
rm -f "$RESULT/setup.txt" "$RESULT/treegrid.txt"
. /etc/os-release
echo "== host: $PRETTY_NAME"
case " $ID ${ID_LIKE:-} " in
  *" fedora "*)
    dnf -y -q install --setopt=install_weak_deps=False \
      xorg-x11-server-Xvfb xorg-x11-xauth xdotool dbus-daemon dbus-tools at-spi2-core \
      gsettings-desktop-schemas python3 python3-gobject-base python3-pyatspi procps-ng findutils util-linux \
      mesa-libGL mesa-libEGL libglvnd-gles mesa-dri-drivers fontconfig dejavu-sans-fonts fribidi libgpg-error \
      > "$RESULT/install.log" 2>&1 || { tail -30 "$RESULT/install.log"; exit 2; }
    ;;
  *" debian "*)
    export DEBIAN_FRONTEND=noninteractive
    { apt-get update && apt-get install -y --no-install-recommends \
      xvfb xauth xdotool dbus dbus-bin at-spi2-core python3 python3-gi gir1.2-atspi-2.0 python3-pyatspi procps \
      libgl1 libegl1 libgles2 libgl1-mesa-dri libfontconfig1 libfreetype6 libharfbuzz0b fonts-dejavu-core \
      libfribidi0 libgpg-error0 libcom-err2; } \
      > "$RESULT/install.log" 2>&1 || { tail -30 "$RESULT/install.log"; exit 2; }
    ;;
  *) echo "unsupported host $ID"; exit 2 ;;
esac
[ -s /etc/machine-id ] || dbus-uuidgen > /etc/machine-id

id -u tester > /dev/null 2>&1 || useradd -m tester
RUN_DIR=/run/user/$(id -u tester)
mkdir -p "$RUN_DIR"; chown tester "$RUN_DIR"; chmod 0700 "$RUN_DIR"
su tester -s /bin/bash -c 'mkdir -p ~/tree/wide && cd ~/tree/wide && for i in $(seq 5000); do : > "f$i"; done'
W=/tmp/treegrid-host
rm -rf "$W"; mkdir -p "$W"
chown -R tester "$W" "$RESULT"
if [ -d "$APPIMAGE" ]; then
  cp -a "$APPIMAGE" "$W/squashfs-root"; chown -R tester "$W/squashfs-root"
else
  (cd "$W" && su tester -s /bin/bash -c "'$APPIMAGE' --appimage-extract > /dev/null")
fi
cat > "$W/session.sh" << EOF
set -uo pipefail
/usr/libexec/at-spi-bus-launcher --launch-immediately > "$RESULT/atspi.log" 2>&1 &
sleep 1
export WEBKIT_DISABLE_COMPOSITING_MODE=1
"$W/squashfs-root/AppRun" > "$RESULT/app.log" 2>&1 &
app=\$!
code=0
python3 "$HERE/treegrid_setup.py" > "$RESULT/setup.txt" 2>&1 || code=10
if [ "\$code" = 0 ]; then
  python3 "$HERE/treegrid_atspi.py" check > "$RESULT/treegrid.txt" 2>&1 || code=\$?
fi
kill -TERM "\$app" 2> /dev/null || true
wait "\$app" 2> /dev/null || true
exit "\$code"
EOF
code=0
su tester -s /bin/bash -c "cd ~ && export XDG_RUNTIME_DIR=$RUN_DIR; xvfb-run -a -s '-screen 0 1440x900x24' dbus-run-session -- bash '$W/session.sh'" \
  > "$RESULT/session.log" 2>&1 || code=$?
cat "$RESULT/setup.txt" 2> /dev/null || true
echo "== the probe"
grep -E '^(RESULT|  FAIL)' "$RESULT/treegrid.txt" 2> /dev/null || tail -20 "$RESULT/treegrid.txt" 2> /dev/null || true
echo "== verdict"
if [ "$code" = 10 ]; then echo "::error::treegrid setup failed (setup.txt)"; exit 1; fi
if [ "$code" = 0 ] && grep -qx 'RESULT: PASS' "$RESULT/treegrid.txt"; then echo "size tree for screen readers (F16): PASS"; exit 0; fi
echo "::error::size tree for screen readers (F16): FAIL (treegrid.txt)"; exit 1
