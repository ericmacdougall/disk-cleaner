#!/bin/sh
# Test-only fake git for the window-close check (check_close.py): first on the app's PATH, so
# the analysis worker runs it for every git command. It records its pid and its child's pid
# (a grandchild of the worker) in $PRUNEDISK_CLOSE_PIDS, then waits, so a git is always
# running while the analysis runs. Core's 30 s git timeout ends it otherwise.
echo "$$" >> "$PRUNEDISK_CLOSE_PIDS"
sleep 300 &
echo "$!" >> "$PRUNEDISK_CLOSE_PIDS"
wait
