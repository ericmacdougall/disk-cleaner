#!/usr/bin/env bash
# The shipped bundles are not end-to-end builds (docs/DESKTOP.md, "End-to-end checks").
# Usage: bash e2e/check-shipped.sh <bundle folder> <scratch folder>
# Unpacks the program from every bundle in the folder (.app, .deb, .AppImage, .msi, NSIS .exe;
# the .dmg is a disk image of the same .app and is not opened) and fails if any program holds a string only an end-to-end build has:
#   PRUNEDISK_E2E_                  the e2e feature's hooks (src-tauri/src/e2e.rs)
#   remote-debugging-port           e2e/windows-automation.conf.json
#   PruneDisk (end-to-end build)    the window title of both automation configs
# Control: e2e/check.sh runs this script on the e2e binary and requires it to fail, naming
# each string that build holds.
# A bundle with no program found in it fails too.
set -euo pipefail
BUNDLE=${1:?bundle folder}
WORK=${2:?scratch folder}
rm -rf "$WORK"
mkdir -p "$WORK"
# Absolute paths: the AppImage is run from inside its scratch folder.
BUNDLE=$(cd "$BUNDLE" && pwd)
WORK=$(cd "$WORK" && pwd)
MARKS=(PRUNEDISK_E2E_ remote-debugging-port 'PruneDisk (end-to-end build)')
checked=0
bad=0

scan() { # <what> <program>
  local m
  echo "checking $1: $2"
  for m in "${MARKS[@]}"; do
    if grep -a -q -F "$m" "$2"; then echo "::error::$1 holds \"$m\""; bad=1; fi
  done
  checked=$((checked + 1))
}
one() { # <what> <folder>: the single program unpacked from one bundle
  local found
  found=$(find "$2" -type f \( -name prunedisk-desktop -o -iname 'prunedisk-desktop.exe' \) | head -1)
  if [ -z "$found" ]; then echo "::error::no program found in $1"; bad=1; return; fi
  scan "$1" "$found"
}

n=0
while IFS= read -r f; do
  n=$((n + 1)); out="$WORK/$n"; mkdir -p "$out"
  case "$f" in
    *.app) one "$f" "$f/Contents/MacOS" ;;
    *.deb) dpkg-deb -x "$f" "$out"; one "$f" "$out" ;;
    *.AppImage)
      chmod +x "$f"
      (cd "$out" && "$f" --appimage-extract > /dev/null)
      one "$f" "$out" ;;
    *.msi)
      # An administrative install only unpacks the files; nothing is installed or registered.
      powershell -NoProfile -Command "\$p = Start-Process msiexec -Wait -PassThru -ArgumentList '/a \"$(cygpath -w "$f")\" /qn TARGETDIR=\"$(cygpath -w "$out")\"'; exit \$p.ExitCode"
      one "$f" "$out" ;;
    *-setup.exe)
      7z x -y -o"$(cygpath -w "$out")" "$(cygpath -w "$f")" > /dev/null
      one "$f" "$out" ;;
    # A loose program: e2e/check.sh's control puts the e2e binary here and expects a failure.
    *) scan "$f" "$f" ;;
  esac
done < <(find "$BUNDLE" -maxdepth 2 \( -name '*.app' -o -name '*.deb' -o -name '*.AppImage' -o -name '*.msi' -o -name '*-setup.exe' \
  -o -type f -name prunedisk-desktop -o -type f -name prunedisk-desktop.exe \) | sort)

echo "programs checked: $checked"
[ "$checked" -gt 0 ] || { echo "::error::no bundle found under $BUNDLE"; exit 1; }
[ "$bad" = 0 ] || exit 1
echo "No shipped program holds an end-to-end build's strings."
