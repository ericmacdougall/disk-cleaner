#!/usr/bin/env bash
# The WebKitGTK inside the Linux AppImage, and the glibc the AppImage needs (docs/DESKTOP.md,
# "Linux AppImage"). Run on the build host right after the AppImage is built:
#   bash src-tauri/appimage/check-webkit.sh <AppImage> <record file>
# The AppImage carries the build host's libwebkit2gtk-4.1, so the host package's version is the
# bundled one; the script proves that by the library's GNU build id. It writes the WebKitGTK
# version, the build host and the highest GLIBC_ symbol version any file in the image needs to
# the record file (shipped next to the bundles), and exits 1 when that WebKitGTK is in the range
# that sends wrong screen reader roles: 2.49.x and 2.50.0 to 2.50.4 dropped Footer from WebKit's
# copy of AT-SPI's role list, so every role from paragraph on reaches the reader one too low
# (a heading as "document frame"; https://bugs.webkit.org/show_bug.cgi?id=300090, fixed in 2.50.5).
set -euo pipefail
# Absolute: the AppImage is run from inside the scratch folder below.
app=$(readlink -f "${1:?AppImage}")
[ -f "$app" ] || { echo "::error::no AppImage at $1"; exit 1; }
record=${2:?record file}
w=$(mktemp -d)
trap 'rm -rf "$w"' EXIT
(cd "$w" && "$app" --appimage-extract > /dev/null)
root="$w/squashfs-root"
lib="$root/usr/lib/libwebkit2gtk-4.1.so.0"
host=$(readlink -f /usr/lib/x86_64-linux-gnu/libwebkit2gtk-4.1.so.0)
build_id() { readelf -n "$1" | awk '/Build ID/ {print $3}'; }
if [ ! -f "$lib" ] || [ -z "$(build_id "$lib")" ] || [ "$(build_id "$lib")" != "$(build_id "$host")" ]; then
  echo "::error::the AppImage's libwebkit2gtk-4.1 is missing or is not the build host's $host"; exit 1
fi
pkg=$(dpkg-query -W -f '${Version}' libwebkit2gtk-4.1-0)
version=${pkg#*:}
version=${version%%-*}
floor=$({ find "$root" -type f -exec objdump -T {} + 2> /dev/null || true; } | grep -oE 'GLIBC_[0-9]+\.[0-9]+' | sort -uV | tail -1)
. /etc/os-release
{
  echo "WebKitGTK $version (libwebkit2gtk-4.1-0 $pkg, built on $PRETTY_NAME)"
  echo "needs glibc ${floor#GLIBC_} or later"
} | tee "$record"
if dpkg --compare-versions "$version" ge 2.49 && dpkg --compare-versions "$version" lt 2.50.5; then
  echo "::error::WebKitGTK $version sends wrong screen reader roles (https://bugs.webkit.org/show_bug.cgi?id=300090, fixed in 2.50.5)"
  exit 1
fi
echo "WebKitGTK $version is outside the range with wrong screen reader roles (2.49 to 2.50.4)"
