#!/usr/bin/env bash
# A screen reader probe of the shipped Linux AppImage, run in a bare container as in
# e2e/appimage-dialog.sh, and its standing control:
#   roles     e2e/a11y/roles_atspi.py on the start screen (every role the page shows)
#   treegrid  e2e/a11y/treegrid_atspi.py on the size tree (F16; CT-F4, CT-F5, gate F6)
#
# Runs as root inside a container, the AppImage and apps/desktop/e2e mounted read-only:
#   docker run --rm --init -v <AppImage>:/bundle/app.AppImage:ro -v apps/desktop/e2e:/e2e:ro \
#     -v <out>:/out fedora:41 bash /e2e/a11y/probe-host.sh treegrid /bundle/app.AppImage /out
#   ... probe-host.sh roles /bundle/app.AppImage /out --expect-shift 1      (the control)
# Fedora and Debian (and derivatives). Steps:
#   1. installs a display, a session and accessibility bus, AT-SPI's Python bindings (gi and
#      pyatspi), xdotool and the libraries an AppImage leaves to the host;
#   2. treegrid only: makes /home/tester/tree/wide with 5,000 empty files (the folder it opens);
#   3. starts the extracted AppRun as the tester under Xvfb and dbus-run-session;
#   4. treegrid only: treegrid_setup.py ticks the / volume, presses Scan, opens the Size tree tab;
#   5. runs the probe's check, with --expect-shift N when given.
#
# Verdict, by the probe's exit code (each probe's docstring lists its codes):
#   no --expect-shift (or 0): PASS only when the probe exits 0.
#   --expect-shift N (N != 0), the control: every expected role is moved by N, so on any build
#     the probe must find wrong roles. PASS ("caught") only when the probe exits 4 (wrong roles,
#     nothing else) and its output names at least one wrong role. Anything else is a FAIL: exit 0
#     (the probe cannot see a wrong role), 3 (it could not check: no app, no page, no tree), 1
#     (other findings, or a crash), a failed setup or install, or a timeout.
# Exit 0 on PASS, 1 on FAIL, 2 on bad usage or a failed install.
# Results in the out folder: probe.txt (the probe's report), setup.txt, app.log, session.log.
set -euo pipefail
PROBE=${1:?roles or treegrid}
APPIMAGE=${2:?AppImage}
RESULT=${3:?out folder}
SHIFT=0
if [ $# -gt 3 ]; then
  [ "$4" = --expect-shift ] && [ $# = 5 ] && [[ $5 =~ ^-?[0-9]+$ ]] || { echo "usage: $0 roles|treegrid APPIMAGE OUT [--expect-shift N]"; exit 2; }
  SHIFT=$5
fi
case "$PROBE" in
  roles) LEG="screen reader roles of the start screen"; WRONG_LINE='^roles: WRONG ' ;;
  treegrid) LEG="size tree for screen readers (F16)"; WRONG_LINE='^  FAIL: .* reaches the reader as ' ;;
  *) echo "unknown probe $PROBE (roles or treegrid)"; exit 2 ;;
esac
[ "$SHIFT" = 0 ] || LEG="$LEG, control with expected roles shifted by $SHIFT"
PROBE_TIMEOUT=${PROBE_TIMEOUT:-600}
HERE=$(cd "$(dirname "$0")" && pwd)
mkdir -p "$RESULT"
rm -f "$RESULT/setup.txt" "$RESULT/probe.txt" "$RESULT/probe.code"
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
if [ "$PROBE" = treegrid ]; then
  su tester -s /bin/bash -c 'mkdir -p ~/tree/wide && cd ~/tree/wide && for i in $(seq 5000); do : > "f$i"; done'
fi
W=/tmp/probe-host
rm -rf "$W"; mkdir -p "$W"
chown -R tester "$W" "$RESULT"
if [ -d "$APPIMAGE" ]; then
  cp -a "$APPIMAGE" "$W/squashfs-root"; chown -R tester "$W/squashfs-root"
else
  (cd "$W" && su tester -s /bin/bash -c "'$APPIMAGE' --appimage-extract > /dev/null")
fi
if [ "$PROBE" = roles ]; then
  SETUP=true
  RUN="python3 '$HERE/roles_atspi.py' check prunedisk --timeout 120 --expect-shift $SHIFT"
else
  SETUP="python3 '$HERE/treegrid_setup.py'"
  RUN="python3 '$HERE/treegrid_atspi.py' check --expect-shift $SHIFT"
fi
# probe.code holds the probe's own exit code; the session's code says only whether setup ran.
cat > "$W/session.sh" << EOF
set -uo pipefail
/usr/libexec/at-spi-bus-launcher --launch-immediately > "$RESULT/atspi.log" 2>&1 &
sleep 1
export WEBKIT_DISABLE_COMPOSITING_MODE=1
"$W/squashfs-root/AppRun" > "$RESULT/app.log" 2>&1 &
app=\$!
code=0
$SETUP > "$RESULT/setup.txt" 2>&1 || code=10
if [ "\$code" = 0 ]; then
  probe=0
  $RUN > "$RESULT/probe.txt" 2>&1 || probe=\$?
  echo "\$probe" > "$RESULT/probe.code"
fi
kill -TERM "\$app" 2> /dev/null || true
wait "\$app" 2> /dev/null || true
exit "\$code"
EOF
code=0
timeout "$PROBE_TIMEOUT" su tester -s /bin/bash -c "cd ~ && export XDG_RUNTIME_DIR=$RUN_DIR; xvfb-run -a -s '-screen 0 1440x900x24' dbus-run-session -- bash '$W/session.sh'" \
  > "$RESULT/session.log" 2>&1 || code=$?
[ "$PROBE" = roles ] || cat "$RESULT/setup.txt" 2> /dev/null || true
echo "== the probe ($PROBE, expected roles shifted by $SHIFT)"
head -60 "$RESULT/probe.txt" 2> /dev/null || echo "(no probe.txt)"
probe=$(cat "$RESULT/probe.code" 2> /dev/null || echo none)
echo "== verdict: session exit $code, probe exit $probe"
fail() { echo "::error::$LEG: FAIL ($1)"; exit 1; }
[ "$code" != 124 ] || fail "timed out after $PROBE_TIMEOUT s"
[ "$code" != 10 ] || fail "setup failed, see setup.txt"
[ "$probe" != none ] || fail "the probe did not run (session exit $code), see session.log"
if [ "$SHIFT" = 0 ]; then
  [ "$probe" = 0 ] || fail "probe exit $probe, see probe.txt"
  echo "$LEG: PASS"; exit 0
fi
case "$probe" in
  4) grep -qE "$WRONG_LINE" "$RESULT/probe.txt" || fail "probe exit 4 but no wrong role named" ;;
  0) fail "probe exit 0: it passed roles that were all wrong, so it cannot catch a wrong role" ;;
  3) fail "probe exit 3: it could not check, so nothing was caught" ;;
  *) fail "probe exit $probe: other findings or a crash, not only wrong roles" ;;
esac
echo "$LEG: PASS (caught: $(grep -cE "$WRONG_LINE" "$RESULT/probe.txt") wrong roles named)"
